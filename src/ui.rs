use crate::core::Cmd;
use crate::docs::FileStatus;
use crate::state::{AppState, AudioMeter, DocsReport, METER_FRAMES, Shared, Speaker};
use eframe::egui::{self, Align, Color32, RichText};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// How long the Clear button waits for the confirming second click.
const CLEAR_CONFIRM: Duration = Duration::from_secs(3);

pub struct App {
    shared: Shared,
    cmd_tx: UnboundedSender<Cmd>,
    hint_max_age: Duration,
    /// Set by the first click on Clear.
    clear_armed: Option<Instant>,
    /// Jump to the newest line on the next frame.
    to_bottom: bool,
    show_report: bool,
    show_audio: bool,
    /// Identifies the report that was last auto-opened, so it pops up once per load.
    report_seen: Option<(Option<String>, usize, usize)>,
}

impl App {
    pub fn new(shared: Shared, cmd_tx: UnboundedSender<Cmd>, hint_max_age: Duration) -> Self {
        Self { shared, cmd_tx, hint_max_age, clear_armed: None, to_bottom: false, show_report: false, show_audio: false, report_seen: None }
    }

    fn send(&self, c: Cmd) {
        let _ = self.cmd_tx.send(c);
    }
}

/// Icons are painted as shapes: the default egui font has no glyphs for ● and ■.
#[derive(Clone, Copy)]
enum Icon {
    Dot,
    Square,
}

/// A button with an optional painted icon followed by text, centered together.
/// `fill` / `text_color` of `None` keep the default button look.
fn icon_button(ui: &mut egui::Ui, icon: Option<Icon>, text: &str, fill: Option<Color32>, text_color: Option<Color32>, size: egui::Vec2) -> egui::Response {
    let mut btn = egui::Button::new("").min_size(size);
    if let Some(f) = fill {
        btn = btn.fill(f);
    }
    let resp = ui.add(btn);
    let color = text_color.unwrap_or_else(|| ui.style().interact(&resp).text_color());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(18.0), color);
    let (d, gap) = (12.0, 8.0);
    let icon_w = if icon.is_some() { d + gap } else { 0.0 };
    let mut x = resp.rect.center().x - (icon_w + galley.size().x) / 2.0;
    let cy = resp.rect.center().y;
    if let Some(icon) = icon {
        let c = egui::pos2(x + d / 2.0, cy);
        match icon {
            Icon::Dot => {
                ui.painter().circle_filled(c, d / 2.0, color);
            }
            Icon::Square => {
                ui.painter().rect_filled(egui::Rect::from_center_size(c, egui::vec2(d, d)), 2.0, color);
            }
        }
        x += icon_w;
    }
    ui.painter().galley(egui::pos2(x, cy - galley.size().y / 2.0), galley, color);
    resp
}

fn age_text(d: Duration) -> String {
    let s = d.as_secs();
    if s < 5 {
        "just now".into()
    } else if s < 60 {
        format!("{s} s ago")
    } else {
        format!("{} min ago", s / 60)
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let AppState {
            segments,
            listening,
            listening_since: since,
            status,
            error,
            conn_error,
            hint,
            pending,
            pending_shown,
            topics,
            topic,
            docs_report,
            log_path,
            topic_loading: loading,
            meters,
            monitoring,
        } = self.shared.read(AppState::clone);

        // A hint prepared in the background and not shown yet: tell the user it is ready.
        let ready = !pending_shown && pending.as_ref().is_some_and(|p| p.started.is_some_and(|t| t.elapsed() < self.hint_max_age));
        let preparing = ready && pending.as_ref().is_some_and(|p| !p.done);

        let mut dismiss_error = false;
        let mut top = egui::TopBottomPanel::top("top");
        if listening {
            // Whole bar tinted red while recording.
            top = top.frame(egui::Frame::side_top_panel(&ctx.style()).fill(Color32::from_rgb(70, 20, 20)));
        }
        top.show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                let btn_size = egui::vec2(140.0, 36.0);
                let (icon, label, fill) = if listening {
                    // Gentle pulse so "listening" is noticeable at a glance.
                    let t = ctx.input(|i| i.time) as f32;
                    let k = 0.5 + 0.5 * (t * std::f32::consts::TAU / 1.2).sin();
                    ctx.request_repaint_after(Duration::from_millis(33));
                    let lerp = |a: f32, b: f32| (a + (b - a) * k) as u8;
                    (Icon::Square, "Stop", Color32::from_rgb(lerp(160.0, 230.0), lerp(30.0, 60.0), lerp(30.0, 60.0)))
                } else {
                    (Icon::Dot, "Listen", Color32::from_rgb(40, 140, 70))
                };
                if icon_button(ui, Some(icon), label, Some(fill), Some(Color32::WHITE), btn_size).clicked() {
                    self.send(if listening { Cmd::Stop } else { Cmd::Start });
                }
                let hint_btn = if ready {
                    icon_button(ui, Some(Icon::Dot), "Hint", Some(Color32::from_rgb(40, 140, 70)), Some(Color32::WHITE), btn_size)
                } else {
                    icon_button(ui, None, "Hint", None, None, btn_size)
                };
                if hint_btn.on_hover_text("Show the hint; press again for a new one").clicked() {
                    self.send(Cmd::AskHint);
                }
                if preparing {
                    ui.spinner();
                }
                let armed = self.clear_armed.is_some_and(|t| t.elapsed() < CLEAR_CONFIRM);
                if !armed {
                    self.clear_armed = None;
                }
                let clear_label = if armed { RichText::new("Clear? (click again)").color(Color32::LIGHT_RED) } else { RichText::new("Clear") };
                if ui.button(clear_label).clicked() {
                    if armed {
                        self.clear_armed = None;
                        self.to_bottom = true;
                        self.send(Cmd::Clear);
                    } else {
                        self.clear_armed = Some(Instant::now());
                        ctx.request_repaint_after(CLEAR_CONFIRM);
                    }
                }
                ui.separator();
                ui.label("Docs:");
                let selected = topic.clone().unwrap_or_else(|| "— select —".into());
                ui.add_enabled_ui(!loading, |ui| {
                    egui::ComboBox::from_id_salt("topic").selected_text(selected).show_ui(ui, |ui| {
                        for t in &topics {
                            if ui.selectable_label(topic.as_deref() == Some(t.as_str()), t).clicked() {
                                self.send(Cmd::SelectTopic(t.clone()));
                            }
                        }
                    });
                });
                if loading {
                    ui.spinner();
                }
                if ui.button("↻").on_hover_text("Rescan the docs folder").clicked() {
                    self.send(Cmd::RefreshTopics);
                }
                ui.separator();
                let mut st = status.clone();
                if let Some(t) = since {
                    let s = t.elapsed().as_secs();
                    st = format!("{st} {:02}:{:02}", s / 60, s % 60);
                    ctx.request_repaint_after(Duration::from_secs(1));
                }
                if listening {
                    let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 5.0, Color32::LIGHT_RED);
                    ui.label(RichText::new(format!("REC {st}")).strong().color(Color32::LIGHT_RED));
                } else {
                    ui.label(st);
                }
            });
            if let Some(e) = &conn_error {
                ui.colored_label(Color32::LIGHT_RED, e);
            }
            if let Some(e) = &error {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(Color32::LIGHT_RED, e);
                    if ui.small_button("x").on_hover_text("Dismiss").clicked() {
                        dismiss_error = true;
                    }
                });
            }
        });
        if dismiss_error {
            self.shared.update(|s| s.error = None);
        }

        // Docs parse quality (left) and log link (right) at the very bottom.
        let problems = docs_report.as_ref().map_or(0, |r| r.files.iter().filter(|f| f.status != FileStatus::Ok).count());
        if let Some(r) = docs_report.as_ref().filter(|_| !loading) {
            let key = (topic.clone(), r.chunks, r.files.len());
            if self.report_seen.as_ref() != Some(&key) {
                self.report_seen = Some(key);
                if problems > 0 {
                    self.show_report = true;
                }
            }
        }
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if loading {
                    ui.spinner();
                    ui.weak("Loading documents…");
                } else if let Some(r) = &docs_report {
                    let failed = r.files.iter().any(|f| matches!(f.status, FileStatus::Failed(_)));
                    let (color, text) = if problems == 0 {
                        (Color32::from_rgb(60, 180, 90), format!("{} PDFs OK · {} chunks", r.files.len(), r.chunks))
                    } else {
                        let c = if failed { Color32::LIGHT_RED } else { Color32::from_rgb(230, 170, 40) };
                        (c, format!("{problems} of {} PDFs with problems · {} chunks", r.files.len(), r.chunks))
                    };
                    let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 5.0, color);
                    let resp = ui.add(egui::Label::new(RichText::new(text).color(color)).sense(egui::Sense::click()));
                    if resp.on_hover_text("Click to show the per-file report").clicked() {
                        self.show_report = !self.show_report;
                    }
                }
                if ui.selectable_label(self.show_audio, "Audio").on_hover_text("Input levels, noise threshold, data sent to OpenAI").clicked() {
                    self.show_audio = !self.show_audio;
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| match &log_path {
                    Some(path) => {
                        if ui.link("Open log").on_hover_text(path.display().to_string()).clicked() {
                            let res = std::process::Command::new("xdg-open").arg(path).spawn();
                            if let Err(e) = res {
                                self.shared.update(|s| s.error = Some(format!("Cannot open {}: {e}", path.display())));
                            }
                        }
                    }
                    None => {
                        ui.weak("Log: terminal");
                    }
                });
            });
        });
        if self.show_report && let Some(r) = &docs_report {
            report_window(ctx, r, &mut self.show_report);
        }

        if self.show_audio {
            let mut open = true;
            audio_window(ctx, &meters, listening, monitoring, &self.cmd_tx, &mut open);
            self.show_audio = open;
        }

        egui::TopBottomPanel::bottom("hint").resizable(true).min_height(180.0).show(ctx, |ui| {
            ui.add_space(4.0);
            egui::ScrollArea::vertical().id_salt("hint_scroll").auto_shrink([false, false]).show(ui, |ui| match &hint {
                None => {
                    ui.weak("The hint will appear here (Hint button).");
                }
                Some(h) => {
                    let age = h.started.map(|t| t.elapsed());
                    let stale = age.is_some_and(|a| a > self.hint_max_age) && h.done;
                    if age.is_some() {
                        ctx.request_repaint_after(Duration::from_secs(1));
                    }
                    // Excerpts that did not hold the answer are not its source: do not list them.
                    let sources = (!h.sources.is_empty() && !h.own).then(|| format!("Sources: {}", h.sources.join(", ")));
                    let meta: Vec<String> = [sources, age.map(age_text)].into_iter().flatten().collect();
                    if !meta.is_empty() {
                        ui.weak(meta.join(" · "));
                    }
                    if !h.question.is_empty() {
                        ui.label(RichText::new(format!("Question: {}", h.question)).size(16.0).color(Color32::GRAY));
                    }
                    let amber = Color32::from_rgb(230, 170, 40);
                    let (es_color, en_color) = match (stale, h.own) {
                        (true, _) => (Color32::GRAY, Color32::from_gray(120)),
                        (false, true) => (amber, Color32::LIGHT_BLUE),
                        (false, false) => (ui.visuals().text_color(), Color32::LIGHT_BLUE),
                    };
                    if h.own {
                        ui.colored_label(amber, "Generated by the model - not found in your documents");
                    }
                    if h.done && h.es.is_empty() {
                        // The model broke the Q/ES/EN format: show what it actually said.
                        let msg = if h.raw.trim().is_empty() { "The model returned an empty answer." } else { h.raw.trim() };
                        ui.label(RichText::new(msg).size(26.0).color(es_color));
                    } else {
                        let mut es = h.es.as_str();
                        ui.add(
                            egui::TextEdit::multiline(&mut es)
                                .font(egui::FontId::proportional(26.0))
                                .text_color(es_color)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .frame(false),
                        );
                        ui.label(RichText::new(&h.en).size(18.0).color(en_color));
                    }
                    if !h.done {
                        ui.spinner();
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let to_bottom = std::mem::take(&mut self.to_bottom);
            let out = egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                for seg in &segments {
                    // Your own speech: marked and tinted, never translated.
                    let me = seg.speaker == Speaker::Me;
                    let text = if me { format!("Me: {}", seg.es) } else { seg.es.clone() };
                    let mut es = RichText::new(text).size(22.0);
                    if me {
                        es = es.color(Color32::from_rgb(120, 200, 140));
                    }
                    if seg.partial {
                        es = es.italics().color(Color32::GRAY);
                    }
                    ui.label(es);
                    if let Some(en) = &seg.en {
                        ui.label(RichText::new(en).size(16.0).color(Color32::from_gray(170)));
                    } else if !me && !seg.partial {
                        // Translation in progress.
                        ui.label(RichText::new("…").size(16.0).color(Color32::from_gray(110)));
                    }
                    ui.add_space(8.0);
                }
                if to_bottom {
                    ui.scroll_to_cursor(Some(Align::BOTTOM));
                }
            });
            let at_bottom = out.state.offset.y + out.inner_rect.height() >= out.content_size.y - 8.0;
            if !at_bottom {
                egui::Area::new(egui::Id::new("latest")).anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -8.0]).show(ctx, |ui| {
                    if ui.button("↓ Latest").clicked() {
                        self.to_bottom = true;
                    }
                });
            }
        });
    }
}

fn report_window(ctx: &egui::Context, r: &DocsReport, open: &mut bool) {
    egui::Window::new("Document report").open(open).default_width(520.0).show(ctx, |ui| {
        egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
            egui::Grid::new("report").num_columns(4).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                ui.strong("File");
                ui.strong("Pages");
                ui.strong("Chunks");
                ui.strong("Status");
                ui.end_row();
                for f in &r.files {
                    ui.label(&f.name);
                    ui.label(f.pages.to_string());
                    ui.label(f.chunks.to_string());
                    match &f.status {
                        FileStatus::Ok => ui.colored_label(Color32::from_rgb(60, 180, 90), "OK"),
                        FileStatus::Warn(m) => ui.colored_label(Color32::from_rgb(230, 170, 40), m),
                        FileStatus::Failed(m) => ui.colored_label(Color32::LIGHT_RED, format!("failed: {m}")),
                    };
                    ui.end_row();
                }
            });
        });
    });
}

fn ago(t: Option<Instant>) -> String {
    t.map_or_else(|| "never".into(), |t| format!("{:.1} s ago", t.elapsed().as_secs_f32()))
}

/// Value below which `frac` of the values lie (`v` must be sorted).
fn percentile(v: &[f32], frac: f32) -> f32 {
    v[((v.len() - 1) as f32 * frac).round() as usize]
}

/// Level bars (newest on the right, log scale) with the threshold line on top.
fn level_chart(ui: &mut egui::Ui, m: &AudioMeter, threshold: f64) {
    const TOP: f32 = 8000.0;
    let y_of = |rms: f32| (1.0 + rms.max(0.0)).ln() / (1.0 + TOP).ln();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 90.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, Color32::from_gray(25));
    let w = rect.width() / METER_FRAMES as f32;
    let n = m.levels.len();
    for (i, (rms, sent)) in m.levels.iter().enumerate() {
        let x = rect.right() - (n - i) as f32 * w;
        let h = y_of(*rms).min(1.0) * rect.height();
        let color = if *sent { Color32::from_rgb(60, 180, 90) } else { Color32::from_gray(110) };
        p.rect_filled(egui::Rect::from_min_max(egui::pos2(x, rect.bottom() - h), egui::pos2(x + w.max(1.0) - 0.5, rect.bottom())), 0.0, color);
    }
    let y = rect.bottom() - y_of(threshold as f32).min(1.0) * rect.height();
    p.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], egui::Stroke::new(1.5_f32, Color32::from_rgb(230, 170, 40)));
}

fn meter_block(ui: &mut egui::Ui, title: &str, m: &AudioMeter, active: bool, listening: bool, suggest_key: &str) {
    ui.strong(title);
    let threshold = crate::audio::threshold_get(&m.threshold);
    level_chart(ui, m, threshold);

    // Noise floor = quiet 10 % of the last ~15 s, peak = the loudest frame.
    let mut sorted: Vec<f32> = m.levels.iter().map(|l| l.0).collect();
    sorted.sort_by(f32::total_cmp);
    let now = m.levels.back().map_or(0.0, |l| l.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("Now: {now:.0}"));
        if !sorted.is_empty() {
            let (floor, peak) = (percentile(&sorted, 0.1), *sorted.last().unwrap());
            ui.label(format!("· noise floor: {floor:.0} · peak: {peak:.0}"));
            let (msg, color) = if !active {
                ("", Color32::GRAY)
            } else if f64::from(floor) >= threshold {
                ("Threshold is below the noise floor: silence is sent (and paid for). Raise it.", Color32::LIGHT_RED)
            } else if f64::from(peak) < threshold {
                ("Nothing passes the threshold: no speech here, or the threshold is too high.", Color32::from_rgb(230, 170, 40))
            } else {
                ("Threshold sits between noise and speech.", Color32::from_rgb(60, 180, 90))
            };
            ui.colored_label(color, msg);
            if peak > floor * 2.0 && active {
                // Deliberately low: passing some noise costs a little, cutting speech loses data.
                let suggest = (f64::from(floor) * 1.5).round().max(20.0);
                ui.weak(format!("Safe value (just above noise): {suggest:.0}  ({suggest_key} = {suggest:.0})"));
            }
        } else {
            ui.weak("No data yet — press Test levels or Listen.");
        }
    });

    let mut t = threshold;
    ui.horizontal(|ui| {
        ui.label("Threshold:");
        if ui.add(egui::Slider::new(&mut t, 20.0..=5000.0).logarithmic(true).integer()).changed() {
            crate::audio::threshold_set(&m.threshold, t);
        }
        ui.weak(format!("{suggest_key} = {t:.0}  (this session only; copy to config.toml)"));
    });

    let pct = if m.frames_total > 0 { 100.0 * m.frames_passed as f64 / m.frames_total as f64 } else { 0.0 };
    let (conn_text, conn_color) = if !listening {
        ("not connected (not listening)", Color32::GRAY)
    } else if m.connected {
        ("connected", Color32::from_rgb(60, 180, 90))
    } else {
        ("no connection", Color32::LIGHT_RED)
    };
    ui.horizontal_wrapped(|ui| {
        let recent = m.last_sent.is_some_and(|t| t.elapsed() < Duration::from_millis(500));
        let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
        ui.painter().circle_filled(dot.center(), 5.0, if recent { Color32::from_rgb(60, 180, 90) } else { Color32::from_gray(80) });
        ui.colored_label(conn_color, conn_text);
        ui.label(format!(
            "· passed gate {pct:.0}% ({}/{} frames) · sent {:.0} KB · last sent {} · last from server {}",
            m.frames_passed,
            m.frames_total,
            m.bytes_sent as f64 / 1024.0,
            ago(m.last_sent),
            ago(m.last_server_msg),
        ));
    });
}

fn audio_window(ctx: &egui::Context, meters: &[AudioMeter; 2], listening: bool, monitoring: bool, cmd_tx: &UnboundedSender<Cmd>, open: &mut bool) {
    ctx.request_repaint_after(Duration::from_millis(100));
    egui::Window::new("Audio").open(open).default_width(620.0).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!listening, |ui| {
                let label = if monitoring { "Stop test" } else { "Test levels" };
                if ui.button(label).on_hover_text("Capture audio only to see levels; nothing is sent to OpenAI").clicked() {
                    let _ = cmd_tx.send(if monitoring { Cmd::MonitorStop } else { Cmd::MonitorStart });
                }
            });
            ui.weak("Green bars are sent to OpenAI, grey are dropped as silence. The orange line is the threshold.");
        });
        ui.separator();
        let active = listening || monitoring;
        meter_block(ui, "Speakers (them)", &meters[Speaker::Them.idx()], active, listening, "silence_threshold");
        ui.separator();
        meter_block(ui, "Microphone (me)", &meters[Speaker::Me.idx()], active, listening, "mic_silence_threshold");
    });
}
