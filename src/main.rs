use std::{
    error::Error,
    fs::File,
    io::{Read, Write},
    os::fd::{AsFd, OwnedFd},
    process::Command,
    sync::Arc,
};

use parley::{
    FontContext, FontFamilyName, Layout, LayoutContext, PositionedLayoutItem, StyleProperty,
};

use vello_cpu::{
    Glyph as VelloGlyph, Pixmap, RenderContext, Resources,
    color::palette::css,
    kurbo::Rect,
    peniko::{Blob, FontData},
};

use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer,
        wl_compositor::{self, WlCompositor},
        wl_keyboard::{self, WlKeyboard},
        wl_registry,
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool,
        wl_surface::{self, WlSurface},
    },
};

use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use xkbcommon_rs::{
    Context as XkbContext, Keymap as XkbKeymap, KeymapFormat as XkbKeymapFormat, State as XkbState,
};

const INITIAL_WIDTH: u16 = 960;
const INITIAL_HEIGHT: u16 = 600;

const MIN_WIDTH: i32 = 480;
const MIN_HEIGHT: i32 = 320;

const FONT_SIZE: f32 = 18.0;

const PADDING_X: f32 = 18.0;
const PADDING_Y: f32 = 16.0;

fn main() -> Result<(), Box<dyn Error>> {
    // 链接 Wayland
    let connection = Connection::connect_to_env()?;
    let mut event_queue = connection.new_event_queue();
    let qh = event_queue.handle();

    connection.display().get_registry(&qh, ());

    let mut app = App::new()?;

    eprintln!("GridTerm started");

    // 事件循环，直接 blocking_dispatch 是不对的
    while app.running {
        event_queue.blocking_dispatch(&mut app)?;
        if app.running && app.configured && app.redraw_requested {
            app.present(&qh)?;
        }
    }

    eprintln!("GridTerm exited");

    Ok(())
}

struct OutstandingBuffer {
    id: u64,
    _proxy: wl_buffer::WlBuffer,
}

struct App {
    running: bool,

    // Wayland
    _compositor: Option<WlCompositor>,
    surface: Option<WlSurface>,
    shm: Option<WlShm>,

    _seat: Option<WlSeat>,
    keyboard: Option<WlKeyboard>,

    wm_base: Option<XdgWmBase>,
    xdg_surface: Option<XdgSurface>,
    toplevel: Option<XdgToplevel>,

    configured: bool,

    // Window
    width: u16,
    height: u16,

    // Grid
    columns: usize,
    rows: usize,

    // Text buffer
    text: Vec<char>,
    cursor: usize,

    // XKB
    xkb_state: Option<XkbState>,

    // Font
    font_family: String,
    font_data: FontData,

    // Parley
    font_context: FontContext,

    // Explicit Brush type:
    // LayoutContext<()> -> Layout<()>.
    layout_context: LayoutContext<()>,

    // Grid metrics
    cell_width: f32,
    cell_height: f32,

    // Vello CPU
    renderer: RenderContext,
    resources: Resources,

    // Redraw
    redraw_requested: bool,

    // Buffer lifecycle
    outstanding_buffers: Vec<OutstandingBuffer>,
    next_buffer_id: u64,
}

impl App {
    fn new() -> Result<Self, Box<dyn Error>> {
        // 寻找系统字体
        let (font_family, font_bytes, face_index) = find_system_monospace_font()?;

        // 加载字体到 vello
        let blob: Blob<u8> = Blob::new(Arc::new(font_bytes));
        let font_data = FontData::new(blob, face_index);

        // Parley contexts.
        let mut font_context = FontContext::new();
        let mut layout_context: LayoutContext<()> = LayoutContext::new();

        // probe 单个字符来获取长宽信息
        let probe_text = "M";
        let mut builder = layout_context.ranged_builder(&mut font_context, probe_text, 1.0, true);
        builder.push_default(StyleProperty::FontSize(FONT_SIZE));
        builder.push_default(StyleProperty::FontFamily(
            FontFamilyName::named(font_family.as_str()).into(),
        ));
        let mut probe: Layout<()> = builder.build(probe_text);
        probe.break_all_lines(None);
        let metrics = {
            let line = probe
                .lines()
                .next()
                .ok_or_else(|| std::io::Error::other("Failed to measure monospace font"))?;
            *line.metrics()
        };

        // Use the actual Parley line advance, not Layout::width.
        let cell_width = metrics.advance.max(1.0);
        let cell_height = metrics.line_height.max(FONT_SIZE * 1.25);

        eprintln!("Grid cell: {:.2} x {:.2}", cell_width, cell_height,);

        let mut app = Self {
            running: true,
            _compositor: None,
            surface: None,
            shm: None,
            _seat: None,
            keyboard: None,
            wm_base: None,
            xdg_surface: None,
            toplevel: None,
            configured: false,
            width: INITIAL_WIDTH,
            height: INITIAL_HEIGHT,
            columns: 1,
            rows: 1,
            text: Vec::new(),
            cursor: 0,
            xkb_state: None,
            font_family,
            font_data,
            font_context,
            layout_context,
            cell_width,
            cell_height,
            renderer: RenderContext::new(INITIAL_WIDTH, INITIAL_HEIGHT),
            resources: Resources::new(),
            redraw_requested: false,
            outstanding_buffers: Vec::new(),
            next_buffer_id: 1,
        };

        app.recalculate_grid();

        Ok(app)
    }

    /// 获取某个 cell 的 pixel origin
    fn cell_origin(&self, row: usize, column: usize) -> (f32, f32) {
        (
            PADDING_X + column as f32 * self.cell_width,
            PADDING_Y + row as f32 * self.cell_height,
        )
    }

    /// 前进 grid 距离，如果到顶或者换行就下一行
    fn advance_grid_position(
        &self,
        row: &mut usize,
        column: &mut usize,
        ch: char,
    ) -> Option<(usize, usize)> {
        if ch == '\n' {
            *row += 1;
            *column = 0;

            return None;
        }

        if *column >= self.columns {
            *row += 1;
            *column = 0;
        }

        let position = (*row, *column);
        *column += 1;
        Some(position)
    }

    /// 计算光标所在位置（非常差实现）
    fn cursor_cell(&self) -> (usize, usize) {
        let mut row = 0usize;
        let mut column = 0usize;

        for ch in self.text.iter().take(self.cursor) {
            self.advance_grid_position(&mut row, &mut column, *ch);
        }

        (row, column)
    }

    /// 重新计算 row column 大小
    fn recalculate_grid(&mut self) {
        let available_width = (self.width as f32 - PADDING_X * 2.0).max(self.cell_width);
        let available_height = (self.height as f32 - PADDING_Y * 2.0).max(self.cell_height);
        self.columns = (available_width / self.cell_width).floor().max(1.0) as usize;
        self.rows = (available_height / self.cell_height).floor().max(1.0) as usize;
    }

    /// 设置窗口大小 => 触发重计算
    fn set_window_size(&mut self, width: i32, height: i32) {
        if width > 0 {
            self.width = width.clamp(320, 4096) as u16;
        }
        if height > 0 {
            self.height = height.clamp(200, 4096) as u16;
        }
        self.recalculate_grid();
        self.redraw_requested = true;
    }

    /// 插入/键入一个字符
    fn insert_char(&mut self, ch: char) {
        let (row, column) = self.cursor_cell();

        // Determine where this character would be placed.
        //
        // Newline always starts the next row.
        // Printable text wraps when the current row is full.
        let target_row = if ch == '\n' || column >= self.columns {
            row + 1
        } else {
            row
        };

        if target_row >= self.rows {
            return;
        }

        self.text.insert(self.cursor, ch);
        self.cursor += 1;

        self.redraw_requested = true;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }

        self.text.remove(self.cursor - 1);
        self.cursor -= 1;

        self.redraw_requested = true;
    }

    /// 处理键盘事件
    fn handle_key(&mut self, key: u32) {
        // Linux evdev keycodes.
        match key {
            1 => {
                // Escape
                self.running = false;
                return;
            }

            14 => {
                // Backspace
                self.backspace();
                return;
            }

            28 => {
                // Enter
                self.insert_char('\n');
                return;
            }

            _ => {}
        }

        // Wayland keycodes are evdev keycodes.
        // XKB keycodes are evdev + 8.
        let decoded = self
            .xkb_state
            .as_ref()
            .and_then(|state| state.key_get_utf8(key + 8))
            .and_then(|bytes| String::from_utf8(bytes).ok());

        let Some(decoded) = decoded else {
            return;
        };

        // v0 only accepts printable ASCII.
        for ch in decoded.chars() {
            if ch.is_ascii_graphic() || ch == ' ' {
                self.insert_char(ch);
            }
        }
    }

    /// install 是将 xkb 加载到当前应用
    fn install_keymap(&mut self, fd: OwnedFd, size: u32) {
        let file = File::from(fd);
        let mut bytes = Vec::with_capacity(size as usize);

        if let Err(error) = file.take(size as u64).read_to_end(&mut bytes) {
            eprintln!("Failed to read XKB keymap: {error}");
            return;
        }

        // The keymap format contains a trailing NUL.
        if let Some(nul) = bytes.iter().position(|b| *b == 0) {
            bytes.truncate(nul);
        }

        let keymap_text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("Invalid XKB keymap: {error}");
                return;
            }
        };
        let context = match XkbContext::new(0) {
            Ok(context) => context,
            Err(error) => {
                eprintln!("Failed to create XKB context: {error}");
                return;
            }
        };
        let keymap =
            match XkbKeymap::new_from_string(context, &keymap_text, XkbKeymapFormat::TextV1, 0) {
                Ok(keymap) => keymap,
                Err(error) => {
                    eprintln!("Failed to parse XKB keymap: {error}");
                    return;
                }
            };

        self.xkb_state = Some(XkbState::new(keymap));

        eprintln!("XKB keymap installed");
    }

    /// 创建窗口
    fn try_create_window(&mut self, qh: &QueueHandle<Self>) {
        if self.xdg_surface.is_some() {
            return;
        }
        let Some(wm_base) = self.wm_base.clone() else {
            return;
        };
        let Some(surface) = self.surface.clone() else {
            return;
        };

        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());

        toplevel.set_title("GridTerm".into());
        toplevel.set_app_id("gridterm".into());
        toplevel.set_min_size(MIN_WIDTH, MIN_HEIGHT);

        // 初始提交，不用 buffer, 等待 configure
        surface.commit();

        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
    }

    /// Parley 字形生成，每个字符单独生成而非整个 terminal
    /// 从而防止多字符 ligature, glymph advance, 等
    fn append_char_glyphs(
        &mut self,
        ch: char,
        row: usize,
        column: usize,
        output: &mut Vec<VelloGlyph>,
    ) {
        if ch == ' ' {
            return;
        }

        let (cell_x, cell_y) = self.cell_origin(row, column);
        let text = ch.to_string();
        let family = self.font_family.clone();

        let mut builder =
            self.layout_context
                .ranged_builder(&mut self.font_context, text.as_str(), 1.0, true);
        builder.push_default(StyleProperty::FontSize(FONT_SIZE));
        builder.push_default(StyleProperty::FontFamily(
            FontFamilyName::named(family.as_str()).into(),
        ));
        let mut layout: Layout<()> = builder.build(text.as_str());
        layout.break_all_lines(None);

        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };

                // Parley's positioned glyph already contains:
                // - run offset
                // - glyph-local x offset
                // - line baseline
                // - glyph-local y offset
                //
                // We only translate it to the Grid cell origin.
                for glyph in glyph_run.positioned_glyphs() {
                    output.push(VelloGlyph {
                        id: glyph.id,
                        x: cell_x + glyph.x,
                        y: cell_y + glyph.y,
                    });
                }
            }
        }
    }

    /// 使用 vello_cpu 进行绘制
    fn render_pixels(&mut self) -> Vec<u8> {
        self.renderer.reset_and_resize(self.width, self.height);

        // 背景
        self.renderer.set_paint(css::BLACK);
        self.renderer
            .fill_rect(&Rect::new(0.0, 0.0, self.width as f64, self.height as f64));

        // 字符
        let mut glyphs: Vec<VelloGlyph> = Vec::new();
        let mut row = 0usize;
        let mut column = 0usize;

        let text_len = self.text.len();
        for index in 0..text_len {
            // Copy char out of Vec<char>.
            // No immutable borrow is held across the iteration.
            let ch = self.text[index];
            let position = self.advance_grid_position(&mut row, &mut column, ch);
            let Some((cell_row, cell_column)) = position else {
                continue;
            };
            if cell_row >= self.rows || cell_column >= self.columns {
                continue;
            }
            self.append_char_glyphs(ch, cell_row, cell_column, &mut glyphs);
        }

        // 绘制字符
        if !glyphs.is_empty() {
            self.renderer.set_paint(css::WHITE);
            _ = self
                .renderer
                .glyph_run(&mut self.resources, &self.font_data)
                .font_size(FONT_SIZE)
                .fill_glyphs(glyphs.into_iter());
        }

        // 绘制光标
        let (cursor_row, cursor_column) = self.cursor_cell();
        if cursor_row < self.rows && cursor_column <= self.columns {
            let (mut x, y) = self.cell_origin(cursor_row, cursor_column.min(self.columns));
            // column == columns means pending wrap.
            // The caret sits at the right edge of the last cell.
            if cursor_column == self.columns {
                x = PADDING_X + self.columns as f32 * self.cell_width;
            }
            self.renderer.set_paint(css::LIME);
            self.renderer.fill_rect(&Rect::new(
                x as f64,
                y as f64,
                (x + 2.0) as f64,
                (y + self.cell_height) as f64,
            ));
        }

        // 渲染到 pixmap
        let mut pixmap = Pixmap::new(self.width, self.height);

        self.renderer.flush();
        self.renderer.render(&mut pixmap, &mut self.resources);

        // RGBA 转换到 wl_shm 格式
        let mut output = Vec::with_capacity(pixmap.data().len() * 4);
        for pixel in pixmap.data() {
            output.extend_from_slice(&[pixel.b, pixel.g, pixel.r, pixel.a]);
        }

        output
    }

    /// Wayland 显示
    fn present(&mut self, qh: &QueueHandle<Self>) -> std::io::Result<()> {
        if !self.configured {
            return Ok(());
        }
        let Some(shm) = self.shm.clone() else {
            return Ok(());
        };
        let Some(surface) = self.surface.clone() else {
            return Ok(());
        };

        let pixels = self.render_pixels();
        let mut file = tempfile::tempfile()?;

        file.write_all(&pixels)?;
        file.flush()?;

        let width = self.width as i32;
        let height = self.height as i32;
        let stride = width * 4;

        let pool_size = i32::try_from(pixels.len())
            .map_err(|_| std::io::Error::other("Wayland SHM buffer too large"))?;
        let pool = shm.create_pool(file.as_fd(), pool_size, qh, ());
        let buffer_id = self.next_buffer_id;
        self.next_buffer_id = self.next_buffer_id.wrapping_add(1);

        let buffer = pool.create_buffer(
            0,
            width,
            height,
            stride,
            wl_shm::Format::Argb8888,
            qh,
            buffer_id,
        );

        // wl_buffer retains the server-side pool reference.
        pool.destroy();
        surface.attach(Some(&buffer), 0, 0);
        surface.damage(0, 0, width, height);
        surface.commit();

        // Keep the proxy until wl_buffer.release.
        self.outstanding_buffers.push(OutstandingBuffer {
            id: buffer_id,
            _proxy: buffer,
        });

        self.redraw_requested = false;

        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name, interface, ..
        } = event
        else {
            return;
        };

        match interface.as_str() {
            "wl_compositor" => {
                if state._compositor.is_some() {
                    return;
                }

                let compositor = registry.bind::<WlCompositor, _, _>(name, 1, qh, ());

                let surface = compositor.create_surface(qh, ());

                state._compositor = Some(compositor);
                state.surface = Some(surface);

                state.try_create_window(qh);
            }

            "wl_shm" => {
                if state.shm.is_some() {
                    return;
                }

                let shm = registry.bind::<WlShm, _, _>(name, 1, qh, ());

                state.shm = Some(shm);

                // In case configure arrived before wl_shm.
                state.redraw_requested = true;
            }

            "wl_seat" => {
                if state._seat.is_some() {
                    return;
                }

                let seat = registry.bind::<WlSeat, _, _>(name, 1, qh, ());

                state._seat = Some(seat);
            }

            "xdg_wm_base" => {
                if state.wm_base.is_some() {
                    return;
                }

                let wm_base = registry.bind::<XdgWmBase, _, _>(name, 1, qh, ());

                state.wm_base = Some(wm_base);

                state.try_create_window(qh);
            }

            _ => {}
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);

impl Dispatch<XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for App {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);

            state.configured = true;
            state.redraw_requested = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for App {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                state.set_window_size(width, height);
            }

            xdg_toplevel::Event::Close => {
                state.running = false;
            }

            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for App {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                let keyboard = seat.get_keyboard(qh, ());

                state.keyboard = Some(keyboard);
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                state.install_keymap(fd, size);
            }

            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb_state) = state.xkb_state.as_mut() {
                    xkb_state.update_mask(
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        0,
                        0,
                        group as usize,
                    );
                }
            }

            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => {
                state.handle_key(key);
            }

            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, u64> for App {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        buffer_id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();

            state
                .outstanding_buffers
                .retain(|entry| entry.id != *buffer_id);
        }
    }
}

// 字体发现
fn find_system_monospace_font() -> Result<(String, Vec<u8>, u32), Box<dyn Error>> {
    let output = Command::new("fc-match")
        .args(["-f", "%{family}\n%{file}\n%{index}\n", "monospace"])
        .output()?;

    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "fc-match failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }

    let output = String::from_utf8(output.stdout)?;

    let mut lines = output.lines();

    let family_line = lines
        .next()
        .ok_or_else(|| std::io::Error::other("Missing font family"))?;

    let font_path = lines
        .next()
        .ok_or_else(|| std::io::Error::other("Missing font path"))?;

    let face_index = lines
        .next()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);

    let family = family_line
        .split(',')
        .next()
        .unwrap_or(family_line)
        .trim()
        .to_owned();

    if family.is_empty() || font_path.trim().is_empty() {
        return Err(std::io::Error::other("Invalid Fontconfig result").into());
    }

    let font_bytes = std::fs::read(font_path)?;

    if font_bytes.is_empty() {
        return Err(std::io::Error::other("Empty font file").into());
    }

    eprintln!(
        "Monospace font: {} ({}, face #{})",
        family, font_path, face_index,
    );

    Ok((family, font_bytes, face_index))
}
