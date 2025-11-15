#![forbid(unsafe_code)]
use anyhow::Result;
use std::collections::HashMap;
use std::env;
use tracing::{Level as TraceLevel, debug, error, info, warn};
use tracing_subscriber::FmtSubscriber;
use x11rb::connection::Connection;
use x11rb::protocol::Event::{self, CreateNotify, DamageNotify, DestroyNotify, PropertyNotify};
use x11rb::protocol::damage::{
    ConnectionExt as DamageExt, Damage, ReportLevel as DamageReportLevel,
};
use x11rb::protocol::render::{
    Color, ConnectionExt as RenderExt, CreatePictureAux, Fixed, PictOp, Pictformat, Picture,
    Transform,
};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperExt;

#[derive(Debug)]
struct Config {
    width: u16,
    height: u16,
    opacity: u32,
    border_size: u16,
    border_color: Color,
    text_x: i16,
    text_y: i16,
    text_foreground: u32,
    text_background: u32,
    hide_when_no_focus: bool,
}

impl Config {
    fn parse_num<T: std::str::FromStr + TryFrom<u128>>(var: &str) -> Option<T>
    where
        <T as TryFrom<u128>>::Error: std::fmt::Debug,
        <T as std::str::FromStr>::Err: std::fmt::Debug,
    {
        if let Ok(s) = env::var(var) {
            let s = s.trim();
            if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))
                && let Ok(n) = u128::from_str_radix(hex, 16)
            {
                return T::try_from(n)
                    .inspect_err(|e| error!("failed to parse '{var}' err={e:?}"))
                    .ok();
            } else {
                return s
                    .parse::<T>()
                    .inspect_err(|e| error!("failed to parse '{var}' err={e:?}"))
                    .ok();
            }
        }
        None
    }

    fn parse_color(var: &str) -> Option<Color> {
        if let Some(raw) = Self::parse_num::<u32>(var) {
            let a = ((raw >> 24) & 0xFF) as u16;
            let r = ((raw >> 16) & 0xFF) as u16;
            let g = ((raw >> 8) & 0xFF) as u16;
            let b = (raw & 0xFF) as u16;

            let scale = |v: u16| (v as f32 / u8::MAX as f32 * u16::MAX as f32) as u16;

            Some(Color {
                red: scale(r),
                green: scale(g),
                blue: scale(b),
                alpha: scale(a),
            })
        } else {
            None
        }
    }

    fn premultiply_argb32(argb: u32) -> u32 {
        let a = (argb >> 24) & 0xFF;
        let r = (argb >> 16) & 0xFF;
        let g = (argb >> 8) & 0xFF;
        let b = argb & 0xFF;

        let r_p = r * a / 255;
        let g_p = g * a / 255;
        let b_p = b * a / 255;

        (a << 24) | (r_p << 16) | (g_p << 8) | b_p
    }

    fn new() -> Self {
        Self {
            width: Self::parse_num("WIDTH").unwrap_or(240),
            height: Self::parse_num("HEIGHT").unwrap_or(135),
            opacity: Self::parse_num("OPACITY").unwrap_or(0xC0000000),
            border_size: Self::parse_num("BORDER_SIZE").unwrap_or(5),
            border_color: Self::parse_color("BORDER_COLOR").unwrap_or(Color {
                red: 0xFFFF,
                green: 0,
                blue: 0,
                alpha: 0x7F00,
            }),
            text_x: Self::parse_num("TEXT_X").unwrap_or(10),
            text_y: Self::parse_num("TEXT_Y").unwrap_or(125),
            text_foreground: Self::premultiply_argb32(
                Self::parse_num("TEXT_FOREGROUND").unwrap_or(0xFF_FF_FF_FF),
            ),
            text_background: Self::premultiply_argb32(
                Self::parse_num("TEXT_BACKGROUND").unwrap_or(0x7F_00_00_00),
            ),
            hide_when_no_focus: env::var("HIDE_WHEN_NO_FOCUS")
                .map(|x| x.parse().unwrap_or(false))
                .unwrap_or(false),
        }
    }
}

#[derive(Debug, Default)]
struct InputState {
    dragging: bool,
    drag_start: (i16, i16),
    win_start: (i16, i16),
}

#[derive(Debug)]
struct Thumbnail<'a> {
    window: Window,
    x: i16,
    y: i16,

    config: &'a Config,
    border_fill: Picture,

    src_picture: Picture,
    dst_picture: Picture,
    overlay_gc: Gcontext,
    overlay_pixmap: Pixmap,
    overlay_picture: Picture,

    character_name: String,
    focused: bool,
    visible: bool,
    minimized: bool,

    src: Window,
    root: Window,
    damage: Damage,
    input_state: InputState,
    conn: &'a RustConnection,
}

impl<'a> Thumbnail<'a> {
    fn new(
        conn: &'a RustConnection,
        screen: &Screen,
        character_name: String,
        src: Window,
        font: Font,
        config: &'a Config,
    ) -> Result<Self> {
        let src_geom = conn.get_geometry(src)?.reply()?;
        let x = src_geom.x + (src_geom.width - config.width) as i16 / 2;
        let y = src_geom.y + (src_geom.height - config.height) as i16 / 2;

        let window = conn.generate_id()?;
        conn.create_window(
            screen.root_depth,
            window,
            screen.root,
            x,
            y,
            config.width,
            config.height,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().override_redirect(1).event_mask(
                EventMask::SUBSTRUCTURE_NOTIFY
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION,
            ),
        )?;

        let opacity_atom = conn
            .intern_atom(false, b"_NET_WM_WINDOW_OPACITY")?
            .reply()?
            .atom;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            opacity_atom,
            AtomEnum::CARDINAL,
            &[config.opacity],
        )?;

        let wm_class = conn.intern_atom(false, b"WM_CLASS")?.reply()?.atom;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            wm_class,
            AtomEnum::STRING,
            b"eve-l-preview\0eve-l-preview\0",
        )?;

        let net_wm_state = conn.intern_atom(false, b"_NET_WM_STATE")?.reply()?.atom;
        let above_atom = conn
            .intern_atom(false, b"_NET_WM_STATE_ABOVE")?
            .reply()?
            .atom;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            net_wm_state,
            AtomEnum::ATOM,
            &[above_atom],
        )?;

        conn.map_window(window)?;

        let border_fill = conn.generate_id()?;
        conn.render_create_solid_fill(border_fill, config.border_color)?;

        let pict_format = get_pictformat(conn, screen.root_depth, false)?;
        let src_picture = conn.generate_id()?;
        let dst_picture = conn.generate_id()?;
        conn.render_create_picture(src_picture, src, pict_format, &CreatePictureAux::new())?;
        conn.render_create_picture(dst_picture, window, pict_format, &CreatePictureAux::new())?;

        let overlay_pixmap = conn.generate_id()?;
        let overlay_picture = conn.generate_id()?;
        conn.create_pixmap(32, overlay_pixmap, screen.root, config.width, config.height)?;
        conn.render_create_picture(
            overlay_picture,
            overlay_pixmap,
            get_pictformat(conn, 32, true)?,
            &CreatePictureAux::new(),
        )?;

        let overlay_gc = conn.generate_id()?;
        conn.create_gc(
            overlay_gc,
            overlay_pixmap,
            &CreateGCAux::new()
                .font(font)
                .foreground(config.text_foreground)
                .background(config.text_background),
        )?;

        let damage = conn.generate_id()?;
        conn.damage_create(damage, src, DamageReportLevel::RAW_RECTANGLES)?;

        let mut _self = Self {
            x,
            y,
            window,
            config,

            border_fill,
            src_picture,
            dst_picture,
            overlay_gc,
            overlay_pixmap,
            overlay_picture,

            character_name,
            focused: false,
            visible: true,
            minimized: false,

            src,
            root: screen.root,
            damage,
            input_state: InputState::default(),
            conn,
        };
        _self.update_name()?;
        Ok(_self)
    }

    fn visibility(&mut self, visible: bool) -> Result<()> {
        if visible == self.visible {
            return Ok(());
        }
        self.visible = visible;
        if visible {
            self.conn.map_window(self.window)?;
        } else {
            self.conn.unmap_window(self.window)?;
        }
        Ok(())
    }

    fn capture(&self) -> Result<()> {
        let geom = self.conn.get_geometry(self.src)?.reply()?;
        let transform = Transform {
            matrix11: to_fixed(geom.width as f32 / self.config.width as f32),
            matrix22: to_fixed(geom.height as f32 / self.config.height as f32),
            matrix33: to_fixed(1.0),
            ..Default::default()
        };
        self.conn
            .render_set_picture_transform(self.src_picture, transform)?;
        self.conn.render_composite(
            PictOp::SRC,
            self.src_picture,
            0u32,
            self.dst_picture,
            0,
            0,
            0,
            0,
            0,
            0,
            self.config.width,
            self.config.height,
        )?;
        Ok(())
    }

    fn border(&self, focused: bool) -> Result<()> {
        if focused {
            self.conn.render_composite(
                PictOp::SRC,
                self.border_fill,
                0u32,
                self.overlay_picture,
                0,
                0,
                0,
                0,
                0,
                0,
                self.config.width,
                self.config.height,
            )?;
        } else {
            self.conn.render_composite(
                PictOp::CLEAR,
                self.overlay_picture,
                0u32,
                self.overlay_picture,
                0,
                0,
                0,
                0,
                0,
                0,
                self.config.width,
                self.config.height,
            )?;
        }
        self.update_name()?;
        Ok(())
    }

    fn minimized(&mut self) -> Result<()> {
        self.minimized = true;
        self.border(false)?;
        let extents = self
            .conn
            .query_text_extents(
                self.overlay_gc,
                b"MINIMIZED"
                    .iter()
                    .map(|&c| Char2b { byte1: 0, byte2: c })
                    .collect::<Vec<_>>()
                    .as_slice(),
            )?
            .reply()?;
        self.conn.image_text8(
            self.overlay_pixmap,
            self.overlay_gc,
            (self.config.width as i16 - extents.overall_width as i16) / 2,
            (self.config.height as i16 + extents.font_ascent + extents.font_descent) / 2,
            b"MINIMIZED",
        )?;
        self.update()?;

        Ok(())
    }

    fn update_name(&self) -> Result<()> {
        self.conn.render_composite(
            PictOp::CLEAR,
            self.overlay_picture,
            0u32,
            self.overlay_picture,
            0,
            0,
            0,
            0,
            self.config.border_size as i16,
            self.config.border_size as i16,
            self.config.width - self.config.border_size * 2,
            self.config.height - self.config.border_size * 2,
        )?;
        self.conn.image_text8(
            self.overlay_pixmap,
            self.overlay_gc,
            self.config.text_x,
            self.config.text_y,
            self.character_name.as_bytes(),
        )?;
        Ok(())
    }

    fn overlay(&self) -> Result<()> {
        self.conn.render_composite(
            PictOp::OVER,
            self.overlay_picture,
            0u32,
            self.dst_picture,
            0,
            0,
            0,
            0,
            0,
            0,
            self.config.width,
            self.config.height,
        )?;
        Ok(())
    }

    fn update(&self) -> Result<()> {
        self.capture()?;
        self.overlay()?;
        Ok(())
    }

    fn focus(&self) -> Result<(), x11rb::errors::ReplyError> {
        let net_active = self
            .conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;

        let ev = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: self.src,
            type_: net_active,
            data: [2, 0, 0, 0, 0].into(),
        };

        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            ev,
        )?;
        self.conn.flush()?;
        info!("focused window: window={}", self.window);
        Ok(())
    }

    fn reposition(&mut self, x: i16, y: i16) -> Result<()> {
        self.conn.configure_window(
            self.window,
            &ConfigureWindowAux::new().x(x as i32).y(y as i32),
        )?;
        self.conn.flush()?;
        self.x = x;
        self.y = y;
        Ok(())
    }

    fn is_hovered(&self, x: i16, y: i16) -> bool {
        x >= self.x
            && x <= self.x + self.config.width as i16
            && y >= self.y
            && y <= self.y + self.config.height as i16
    }
}

impl Drop for Thumbnail<'_> {
    fn drop(&mut self) {
        if let Err(e) = (|| {
            self.conn.damage_destroy(self.damage)?;
            self.conn.free_gc(self.overlay_gc)?;
            self.conn.render_free_picture(self.overlay_picture)?;
            self.conn.render_free_picture(self.src_picture)?;
            self.conn.render_free_picture(self.dst_picture)?;
            self.conn.render_free_picture(self.border_fill)?;
            self.conn.free_pixmap(self.overlay_pixmap)?;
            self.conn.destroy_window(self.window)?;
            self.conn.flush()?;
            Ok::<(), anyhow::Error>(())
        })() {
            error!("error during thumbnail drop: {e:?}");
        }
    }
}

fn to_fixed(v: f32) -> Fixed {
    (v * (1 << 16) as f32).round() as Fixed
}

#[tracing::instrument]
fn get_pictformat(conn: &RustConnection, depth: u8, alpha: bool) -> Result<Pictformat> {
    if let Some(format) = conn
        .render_query_pict_formats()?
        .reply()?
        .formats
        .iter()
        .find(|format| {
            debug!(
                "discovered Pictformat: {}, {}",
                format.depth, format.direct.alpha_mask
            );
            format.depth == depth
                && if alpha {
                    format.direct.alpha_mask != 0
                } else {
                    format.direct.alpha_mask == 0
                }
        })
    {
        debug!(
            "using Pictformat: {}, {}",
            format.depth, format.direct.alpha_mask
        );
        Ok(format.id)
    } else {
        anyhow::bail!("could not find suitable Pictformat")
    }
}

fn is_window_eve(conn: &RustConnection, window: Window) -> Result<Option<String>> {
    let wm_name = conn.intern_atom(false, b"WM_NAME")?.reply()?.atom;
    let name_prop = conn
        .get_property(false, window, wm_name, AtomEnum::STRING, 0, 1024)?
        .reply()?;
    let title = String::from_utf8_lossy(&name_prop.value).into_owned();
    Ok(if let Some(name) = title.strip_prefix("EVE - ") {
        Some(name.to_string())
    } else if title == "EVE" {
        Some(String::new())
    } else {
        None
    })
}

fn check_and_create_window<'a>(
    conn: &'a RustConnection,
    screen: &Screen,
    config: &'a Config,
    window: Window,
) -> Result<Option<Thumbnail<'a>>> {
    let pid_atom = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
    if let Ok(prop) = conn
        .get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1)?
        .reply()
    {
        if !prop.value.is_empty() {
            let pid = u32::from_ne_bytes(prop.value[0..4].try_into()?);
            if !std::fs::read_link(format!("/proc/{pid}/exe"))
                .map(|x| {
                    x.to_string_lossy().contains("wine64-preloader")
                        || x.to_string_lossy().contains("wine-preloader")
                })
                .inspect_err(|e| {
                    error!("cant read link '/proc/{pid}/exe' assuming its wine: err={e:?}")
                })
                .unwrap_or(true)
            {
                return Ok(None); // Return if we can determine that the window is not running through wine.
            }
        } else {
            warn!("_NET_WM_PID not set for window={window} assuming its wine");
        }
    }

    conn.change_window_attributes(
        window,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;

    if let Some(character_name) = is_window_eve(conn, window)? {
        conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE | EventMask::FOCUS_CHANGE),
        )?;

        let font = conn.generate_id()?;
        conn.open_font(font, b"fixed")?;
        let thumbnail = Thumbnail::new(conn, screen, character_name, window, font, config)?;
        conn.close_font(font)?;
        info!("constructed Thumbnail for eve window: window={window}");
        Ok(Some(thumbnail))
    } else {
        Ok(None)
    }
}

fn get_eves<'a>(
    conn: &'a RustConnection,
    screen: &Screen,
    config: &'a Config,
) -> Result<HashMap<Window, Thumbnail<'a>>> {
    let net_client_list = conn.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
    let prop = conn
        .get_property(
            false,
            screen.root,
            net_client_list,
            AtomEnum::WINDOW,
            0,
            u32::MAX,
        )?
        .reply()?;
    let windows: Vec<u32> = prop
        .value32().map(|x| x.collect()).unwrap_or_else(|| vec![]);

    let mut eves = HashMap::new();
    for w in windows {
        if let Some(eve) = check_and_create_window(conn, screen, config, w)? {
            eves.insert(w, eve);
        }
    }
    conn.flush()?;
    Ok(eves)
}

fn handle_event<'a>(
    conn: &'a RustConnection,
    screen: &Screen,
    config: &'a Config,
    eves: &mut HashMap<Window, Thumbnail<'a>>,
    event: Event,
) -> Result<()> {
    match event {
        DamageNotify(event) => {
            if let Some(thumbnail) = eves
                .values()
                .find(|thumbnail| thumbnail.damage == event.damage)
            {
                thumbnail.update()?; // TODO: add fps limiter?
                conn.damage_subtract(event.damage, 0u32, 0u32)?;
                conn.flush()?;
            }
        }
        CreateNotify(event) => {
            if let Some(thumbnail) = check_and_create_window(conn, screen, config, event.window)? {
                eves.insert(event.window, thumbnail);
            }
        }
        DestroyNotify(event) => {
            eves.remove(&event.window);
        }
        PropertyNotify(event) => {
            let wm_name = conn.intern_atom(false, b"WM_NAME")?.reply()?.atom;
            let net_wm_state = conn.intern_atom(false, b"_NET_WM_STATE")?.reply()?.atom;
            let net_wm_state_hidden = conn
                .intern_atom(false, b"_NET_WM_STATE_HIDDEN")?
                .reply()?
                .atom;
            if event.atom == wm_name
                && let Some(thumbnail) = eves.get_mut(&event.window)
                && let Some(character_name) = is_window_eve(conn, event.window)?
            {
                thumbnail.character_name = character_name;
                thumbnail.update_name()?;
            } else if event.atom == wm_name
                && let Some(thumbnail) =
                    check_and_create_window(conn, screen, config, event.window)?
            {
                eves.insert(event.window, thumbnail);
            } else if event.atom == net_wm_state
                && let Some(thumbnail) = eves.get_mut(&event.window)
                && let Some(state) = conn
                    .get_property(false, event.window, event.atom, AtomEnum::ATOM, 0, 1024)?
                    .reply()?
                    .value32()
                && state.collect::<Vec<_>>().contains(&net_wm_state_hidden)
            {
                thumbnail.minimized()?;
            }
        }
        Event::FocusIn(event) => {
            if let Some(thumbnail) = eves.get_mut(&event.event) {
                thumbnail.minimized = false;
                thumbnail.focused = true;
                thumbnail.border(true)?;
                if config.hide_when_no_focus && eves.values().any(|x| !x.visible) {
                    for thumbnail in eves.values_mut() {
                        thumbnail.visibility(true)?;
                    }
                }
            }
        }
        Event::FocusOut(event) => {
            if let Some(thumbnail) = eves.get_mut(&event.event) {
                thumbnail.focused = false;
                thumbnail.border(false)?;
                if config.hide_when_no_focus && eves.values().all(|x| !x.focused && !x.minimized) {
                    for thumbnail in eves.values_mut() {
                        thumbnail.visibility(false)?;
                    }
                }
            }
        }
        Event::ButtonPress(event) => {
            if let Some((_, thumbnail)) = eves
                .iter_mut()
                .find(|(_, thumb)| thumb.visible && thumb.is_hovered(event.root_x, event.root_y))
            {
                let geom = conn.get_geometry(thumbnail.window)?.reply()?;
                thumbnail.input_state.drag_start = (event.root_x, event.root_y);
                thumbnail.input_state.win_start = (geom.x, geom.y);
                thumbnail.input_state.dragging = true;
            }
        }
        Event::ButtonRelease(event) => {
            if let Some((_, thumbnail)) = eves.iter_mut().find(|(_, thumb)| {
                thumb.visible
                    && thumb.input_state.dragging
                    && thumb.is_hovered(event.root_x, event.root_y)
            }) {
                if event.detail == 1
                    && thumbnail.input_state.drag_start == (event.root_x, event.root_y)
                {
                    thumbnail.focus()?;
                }
                thumbnail.input_state.dragging = false;
            }
        }
        Event::MotionNotify(event) => {
            if let Some((_, thumbnail)) = eves.iter_mut().find(|(_, thumb)| {
                thumb.visible
                    && thumb.input_state.dragging
                    && thumb.is_hovered(event.root_x, event.root_y)
            }) {
                // TODO: snap to be inline with other thumbnails
                let dx = event.root_x - thumbnail.input_state.drag_start.0;
                let dy = event.root_y - thumbnail.input_state.drag_start.1;
                let new_x = thumbnail.input_state.win_start.0 + dx;
                let new_y = thumbnail.input_state.win_start.1 + dy;
                thumbnail.reposition(new_x, new_y)?;
            }
        }
        _ => (),
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(TraceLevel::INFO)
        .finish();

    tracing::subscriber::set_global_default(subscriber)?;

    let config = Config::new();
    info!("config={config:#?}");

    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    conn.damage_query_version(1, 1)?;
    conn.change_window_attributes(
        screen.root,
        &ChangeWindowAttributesAux::new().event_mask(
            EventMask::SUBSTRUCTURE_NOTIFY
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::POINTER_MOTION,
        ),
    )?;
    info!("successfully connected to x11: screen={screen_num}");

    let mut eves = get_eves(&conn, screen, &config)?;
    loop {
        let event = conn.wait_for_event()?;
        let _ = handle_event(&conn, screen, &config, &mut eves, event)
            .inspect_err(|err| error!("ecountered error in 'handle_event': err={err:#?}"));
    }
}
