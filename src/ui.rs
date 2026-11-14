use crate::core::Cmd;
use crate::state::{AppState, Shared, Speaker};
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
}

impl App {
    pub fn new(shared: Shared, cmd_tx: UnboundedSender<Cmd>, hint_max_age: Duration) -> Self {
        Self { shared, cmd_tx, hint_max_age, clear_armed: None, to_bottom: false }
    }

    fn send(&self, c: Cmd) {
        let _ = self.cmd_tx.send(c);
    }
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
            topic_info,
            topic_loading: loading,
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
                let (label, fill) = if listening {
                    // Gentle pulse so "listening" is noticeable at a glance.
                    let t = ctx.input(|i| i.time) as f32;
                    let k = 0.5 + 0.5 * (t * std::f32::consts::TAU / 1.2).sin();
                    ctx.request_repaint_after(Duration::from_millis(33));
                    let lerp = |a: f32, b: f32| (a + (b - a) * k) as u8;
                    ("■ Stop", Color32::from_rgb(lerp(160.0, 230.0), lerp(30.0, 60.0), lerp(30.0, 60.0)))
                } else {
                    ("● Listen", Color32::from_rgb(40, 140, 70))
                };
                let listen_btn = egui::Button::new(RichText::new(label).size(18.0).strong().color(Color32::WHITE)).fill(fill).min_size(btn_size);
                if ui.add(listen_btn).clicked() {
                    self.send(if listening { Cmd::Stop } else { Cmd::Start });
                }
                let hint_label = if ready { "Hint ●" } else { "Hint" };
                let mut hint_btn = egui::Button::new(RichText::new(hint_label).size(18.0)).min_size(btn_size);
                if ready {
                    hint_btn = egui::Button::new(RichText::new(hint_label).size(18.0).strong().color(Color32::WHITE))
                        .fill(Color32::from_rgb(40, 140, 70))
                        .min_size(btn_size);
                }
                if ui.add(hint_btn).on_hover_text("Show the hint; press again for a new one").clicked() {
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
                } else if let Some(info) = &topic_info {
                    ui.weak(info);
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
                    ui.label(RichText::new(format!("● REC {st}")).strong().color(Color32::LIGHT_RED));
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
                    if ui.small_button("✕").on_hover_text("Dismiss").clicked() {
                        dismiss_error = true;
                    }
                });
            }
        });
        if dismiss_error {
            self.shared.update(|s| s.error = None);
        }

        egui::TopBottomPanel::bottom("hint").resizable(true).min_height(130.0).show(ctx, |ui| {
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
                    let sources = (!h.sources.is_empty()).then(|| format!("Sources: {}", h.sources.join(", ")));
                    let meta: Vec<String> = [sources, age.map(age_text)].into_iter().flatten().collect();
                    if !meta.is_empty() {
                        ui.weak(meta.join(" · "));
                    }
                    if !h.question.is_empty() {
                        ui.label(RichText::new(format!("Question: {}", h.question)).color(Color32::GRAY));
                    }
                    let (es_color, en_color) = if stale { (Color32::GRAY, Color32::from_gray(120)) } else { (ui.visuals().text_color(), Color32::LIGHT_BLUE) };
                    if h.done && h.es.is_empty() {
                        // The model broke the Q/ES/EN format: show what it actually said.
                        let msg = if h.raw.trim().is_empty() { "The model returned an empty answer." } else { h.raw.trim() };
                        ui.label(RichText::new(msg).size(20.0).color(es_color));
                    } else {
                        let mut es = h.es.as_str();
                        ui.add(
                            egui::TextEdit::multiline(&mut es)
                                .font(egui::FontId::proportional(24.0))
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
                        ui.label(RichText::new(en).size(16.0).color(Color32::from_gray(150)));
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
