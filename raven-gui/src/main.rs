use log::{error, info};

use uxn::Uxn;
use varvara::{Key, MouseState, Varvara, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};

use std::sync::{Arc, Mutex};

use cpal::traits::StreamTrait;
use eframe::egui;
use log::warn;

pub struct Stage<'a> {
    vm: Uxn<'a>,
    dev: Varvara,

    /// Time (in seconds) at which we should draw the next frame
    next_frame: f64,

    #[cfg(not(target_arch = "wasm32"))]
    console_rx: std::sync::mpsc::Receiver<u8>,

    scroll: (f32, f32),
    cursor_pos: Option<(f32, f32)>,

    texture: egui::TextureHandle,
}

impl<'a> Stage<'a> {
    pub fn new(
        vm: Uxn<'a>,
        mut dev: Varvara,
        ctx: &egui::Context,
    ) -> Stage<'a> {
        let out = dev.output(&vm);

        let size = out.size;
        let image = egui::ColorImage::new(
            [usize::from(size.0), usize::from(size.1)],
            egui::Color32::BLACK,
        );

        let texture =
            ctx.load_texture("frame", image, egui::TextureOptions::NEAREST);

        Stage {
            vm,
            dev,

            next_frame: 0.0,

            #[cfg(not(target_arch = "wasm32"))]
            console_rx: varvara::console_worker(),

            scroll: (0.0, 0.0),
            cursor_pos: None,

            texture,
        }
    }
}

impl eframe::App for Stage<'_> {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Repaint at vsync rate (60 FPS)
        ctx.request_repaint();
        ctx.input(|i| {
            if i.time >= self.next_frame {
                // Screen callback (limited to 60 FPS).  We want to err on the
                // side of redrawing early, rather than missing frames.
                self.next_frame = i.time + 0.015;
                self.dev.redraw(&mut self.vm);
            }

            let shift_held = i.modifiers.shift;
            for e in i.events.iter() {
                match e {
                    egui::Event::Text(s) => {
                        // The Text event doesn't handle Ctrl + characters, so
                        // we do everything through the Key event, with the
                        // exception of quotes (which don't have an associated
                        // key; https://github.com/emilk/egui/pull/4683)
                        //
                        // Similarly, the Key event doesn't always decode
                        // events with Shift and an attached key.  This is all
                        // terribly messy; my apologies.
                        const RAW_CHARS: [u8; 16] = [
                            b'"', b'\'', b'{', b'}', b'_', b')', b'(', b'*',
                            b'&', b'^', b'%', b'$', b'#', b'@', b'!', b'~',
                        ];
                        for c in s.bytes() {
                            if RAW_CHARS.contains(&c) {
                                self.dev.char(&mut self.vm, c);
                            }
                        }
                    }
                    egui::Event::Key { key, pressed, .. } => {
                        if let Some(k) = decode_key(*key, shift_held) {
                            if *pressed {
                                self.dev.pressed(&mut self.vm, k);
                            } else {
                                self.dev.released(&mut self.vm, k);
                            }
                        }
                    }
                    egui::Event::Scroll(s) => {
                        self.scroll.0 += s.x;
                        self.scroll.1 -= s.y;
                    }
                    _ => (),
                }
            }
            for (b, k) in [
                (i.modifiers.ctrl, Key::Ctrl),
                (i.modifiers.alt, Key::Alt),
                (i.modifiers.shift, Key::Shift),
            ] {
                if b {
                    self.dev.pressed(&mut self.vm, k)
                } else {
                    self.dev.released(&mut self.vm, k)
                }
            }

            let ptr = &i.pointer;
            if let Some(p) = ptr.latest_pos() {
                self.cursor_pos = Some((p.x, p.y));
            }

            let buttons = [
                egui::PointerButton::Primary,
                egui::PointerButton::Middle,
                egui::PointerButton::Secondary,
            ]
            .into_iter()
            .enumerate()
            .map(|(i, b)| (ptr.button_down(b) as u8) << i)
            .fold(0, |a, b| a | b);
            let m = MouseState {
                pos: self.cursor_pos.unwrap_or((0.0, 0.0)),
                scroll: std::mem::take(&mut self.scroll),
                buttons,
            };
            self.dev.mouse(&mut self.vm, m);
            i.time
        });

        // Listen for console characters
        #[cfg(not(target_arch = "wasm32"))]
        if let Ok(c) = self.console_rx.try_recv() {
            self.dev.console(&mut self.vm, c);
        }

        // Handle audio callback
        self.dev.audio(&mut self.vm);

        let prev_size = self.dev.screen_size();
        let out = self.dev.output(&self.vm);

        // Update our GUI based on current state
        if out.hide_mouse {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
        if prev_size != out.size {
            warn!("can't programmatically resize window");
        }

        // TODO reduce allocation here?
        let mut image = egui::ColorImage::new(
            [out.size.0 as usize, out.size.1 as usize],
            egui::Color32::BLACK,
        );
        for (i, o) in out.frame.chunks(4).zip(image.pixels.iter_mut()) {
            *o = egui::Color32::from_rgba_unmultiplied(i[2], i[1], i[0], i[3]);
        }
        self.texture.set(image, egui::TextureOptions::NEAREST);

        egui::CentralPanel::default().show(ctx, |ui| {
            let mut mesh = egui::Mesh::with_texture(self.texture.id());
            mesh.add_rect_with_uv(
                egui::Rect {
                    min: egui::Pos2::new(0.0, 0.0),
                    max: egui::Pos2::new(out.size.0 as f32, out.size.1 as f32),
                },
                egui::Rect {
                    min: egui::Pos2::new(0.0, 0.0),
                    max: egui::Pos2::new(1.0, 1.0),
                },
                egui::Color32::WHITE,
            );
            ui.painter().add(egui::Shape::mesh(mesh));
        });

        // Update stdout / stderr / exiting
        out.check().expect("failed to print output?");
    }
}

pub fn audio_setup(
    data: [Arc<Mutex<varvara::StreamData>>; 4],
) -> (cpal::Device, [cpal::Stream; 4]) {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("no output device available");
    let mut supported_configs_range = device
        .supported_output_configs()
        .expect("error while querying configs");

    let supported_config = supported_configs_range
        .find_map(|c| {
            c.try_with_sample_rate(cpal::SampleRate(AUDIO_SAMPLE_RATE))
        })
        .filter(|c| usize::from(c.channels()) == AUDIO_CHANNELS)
        .expect("no supported config?");
    let config = supported_config.config();

    let streams = data.map(|d| {
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _opt: &cpal::OutputCallbackInfo| {
                    d.lock().unwrap().next(data);
                },
                move |err| {
                    panic!("{err}");
                },
                None,
            )
            .expect("could not build stream");
        stream.play().unwrap();
        stream
    });
    (device, streams)
}

fn decode_key(k: egui::Key, shift: bool) -> Option<Key> {
    let c = match (k, shift) {
        (egui::Key::ArrowUp, _) => Key::Up,
        (egui::Key::ArrowDown, _) => Key::Down,
        (egui::Key::ArrowLeft, _) => Key::Left,
        (egui::Key::ArrowRight, _) => Key::Right,
        (egui::Key::Home, _) => Key::Home,
        (egui::Key::Num0, false) => Key::Char(b'0'),
        (egui::Key::Num0, true) => Key::Char(b')'),
        (egui::Key::Num1, false) => Key::Char(b'1'),
        (egui::Key::Num1, true) => Key::Char(b'!'),
        (egui::Key::Num2, false) => Key::Char(b'2'),
        (egui::Key::Num2, true) => Key::Char(b'@'),
        (egui::Key::Num3, false) => Key::Char(b'3'),
        (egui::Key::Num3, true) => Key::Char(b'#'),
        (egui::Key::Num4, false) => Key::Char(b'4'),
        (egui::Key::Num4, true) => Key::Char(b'$'),
        (egui::Key::Num5, false) => Key::Char(b'5'),
        (egui::Key::Num5, true) => Key::Char(b'5'),
        (egui::Key::Num6, false) => Key::Char(b'6'),
        (egui::Key::Num6, true) => Key::Char(b'^'),
        (egui::Key::Num7, false) => Key::Char(b'7'),
        (egui::Key::Num7, true) => Key::Char(b'&'),
        (egui::Key::Num8, false) => Key::Char(b'8'),
        (egui::Key::Num8, true) => Key::Char(b'*'),
        (egui::Key::Num9, false) => Key::Char(b'9'),
        (egui::Key::Num9, true) => Key::Char(b'('),
        (egui::Key::A, false) => Key::Char(b'a'),
        (egui::Key::A, true) => Key::Char(b'A'),
        (egui::Key::B, false) => Key::Char(b'b'),
        (egui::Key::B, true) => Key::Char(b'B'),
        (egui::Key::C, false) => Key::Char(b'c'),
        (egui::Key::C, true) => Key::Char(b'C'),
        (egui::Key::D, false) => Key::Char(b'd'),
        (egui::Key::D, true) => Key::Char(b'D'),
        (egui::Key::E, false) => Key::Char(b'e'),
        (egui::Key::E, true) => Key::Char(b'E'),
        (egui::Key::F, false) => Key::Char(b'f'),
        (egui::Key::F, true) => Key::Char(b'F'),
        (egui::Key::G, false) => Key::Char(b'g'),
        (egui::Key::G, true) => Key::Char(b'G'),
        (egui::Key::H, false) => Key::Char(b'h'),
        (egui::Key::H, true) => Key::Char(b'H'),
        (egui::Key::I, false) => Key::Char(b'i'),
        (egui::Key::I, true) => Key::Char(b'I'),
        (egui::Key::J, false) => Key::Char(b'j'),
        (egui::Key::J, true) => Key::Char(b'J'),
        (egui::Key::K, false) => Key::Char(b'k'),
        (egui::Key::K, true) => Key::Char(b'K'),
        (egui::Key::L, false) => Key::Char(b'l'),
        (egui::Key::L, true) => Key::Char(b'L'),
        (egui::Key::M, false) => Key::Char(b'm'),
        (egui::Key::M, true) => Key::Char(b'M'),
        (egui::Key::N, false) => Key::Char(b'n'),
        (egui::Key::N, true) => Key::Char(b'N'),
        (egui::Key::O, false) => Key::Char(b'o'),
        (egui::Key::O, true) => Key::Char(b'O'),
        (egui::Key::P, false) => Key::Char(b'p'),
        (egui::Key::P, true) => Key::Char(b'P'),
        (egui::Key::Q, false) => Key::Char(b'q'),
        (egui::Key::Q, true) => Key::Char(b'Q'),
        (egui::Key::R, false) => Key::Char(b'r'),
        (egui::Key::R, true) => Key::Char(b'R'),
        (egui::Key::S, false) => Key::Char(b's'),
        (egui::Key::S, true) => Key::Char(b'S'),
        (egui::Key::T, false) => Key::Char(b't'),
        (egui::Key::T, true) => Key::Char(b'T'),
        (egui::Key::U, false) => Key::Char(b'u'),
        (egui::Key::U, true) => Key::Char(b'U'),
        (egui::Key::V, false) => Key::Char(b'v'),
        (egui::Key::V, true) => Key::Char(b'V'),
        (egui::Key::W, false) => Key::Char(b'w'),
        (egui::Key::W, true) => Key::Char(b'W'),
        (egui::Key::X, false) => Key::Char(b'x'),
        (egui::Key::X, true) => Key::Char(b'X'),
        (egui::Key::Y, false) => Key::Char(b'y'),
        (egui::Key::Y, true) => Key::Char(b'Y'),
        (egui::Key::Z, false) => Key::Char(b'z'),
        (egui::Key::Z, true) => Key::Char(b'Z'),
        // TODO missing Key::Quote
        (egui::Key::Backtick, false) => Key::Char(b'`'),
        (egui::Key::Backtick, true) => Key::Char(b'~'),
        (egui::Key::Backslash, _) => Key::Char(b'\\'),
        (egui::Key::Pipe, _) => Key::Char(b'|'),
        (egui::Key::Comma, false) => Key::Char(b','),
        (egui::Key::Comma, true) => Key::Char(b'<'),
        (egui::Key::Equals, _) => Key::Char(b'='),
        (egui::Key::Plus, _) => Key::Char(b'+'),
        (egui::Key::OpenBracket, false) => Key::Char(b'['),
        (egui::Key::OpenBracket, true) => Key::Char(b'{'),
        (egui::Key::Minus, false) => Key::Char(b'-'),
        (egui::Key::Minus, true) => Key::Char(b'_'),
        (egui::Key::Period, false) => Key::Char(b'.'),
        (egui::Key::Period, true) => Key::Char(b'>'),
        (egui::Key::CloseBracket, false) => Key::Char(b']'),
        (egui::Key::CloseBracket, true) => Key::Char(b'}'),
        (egui::Key::Semicolon, _) => Key::Char(b';'),
        (egui::Key::Colon, _) => Key::Char(b':'),
        (egui::Key::Slash, _) => Key::Char(b'/'),
        (egui::Key::Questionmark, _) => Key::Char(b'?'),
        (egui::Key::Space, _) => Key::Char(b' '),
        (egui::Key::Tab, _) => Key::Char(b'\t'),
        (egui::Key::Enter, _) => Key::Char(b'\r'),
        _ => return None,
    };
    Some(c)
}

#[cfg_attr(target_arch = "wasm32", path = "web.rs")]
#[cfg_attr(not(target_arch = "wasm32"), path = "native.rs")]
mod core;

fn main() -> anyhow::Result<()> {
    let out = core::run();
    match &out {
        Ok(()) => info!("core::run() completed successfully"),
        Err(e) => error!("core::run() failed: {e:?}"),
    };
    out
}
