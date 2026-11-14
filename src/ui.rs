use crate::core::Cmd;
use crate::state::{Shared, Speaker};
use eframe::egui::{self, Color32, Key, Modifiers, RichText};
use tokio::sync::mpsc::UnboundedSender;

pub struct App {
    shared: Shared,
    cmd_tx: UnboundedSender<Cmd>,
}

impl App {
    pub fn new(shared: Shared, cmd_tx: UnboundedSender<Cmd>) -> Self {
        Self { shared, cmd_tx }
    }

    fn send(&self, c: Cmd) {
        let _ = self.cmd_tx.send(c);
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Hotkeys (consumed before widgets so Space does not click a focused button).
        let space = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Space));
        let h = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::H));

        let (segments, listening, since, status, error, hint, topics, topic, loading) = self.shared.read(|s| {
            (s.segments.clone(), s.listening, s.listening_since, s.status.clone(), s.error.clone(), s.hint.clone(), s.topics.clone(), s.topic.clone(), s.topic_loading)
        });

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let label = if listening { "■ Stop (Space)" } else { "● Listen (Space)" };
                if ui.button(RichText::new(label).size(18.0)).clicked() || space {
                    self.send(if listening { Cmd::Stop } else { Cmd::Start });
                }
                if ui.button(RichText::new("Hint (H)").size(18.0)).clicked() || h {
                    self.send(Cmd::AskHint);
                }
                if ui.button("Clear").clicked() {
                    self.send(Cmd::Clear);
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
                if ui.button("↻").on_hover_text("Rescan the docs folder").clicked() {
                    self.send(Cmd::RefreshTopics);
                }
                ui.separator();
                let mut st = status.clone();
                if let Some(t) = since {
                    let s = t.elapsed().as_secs();
                    st = format!("{st} {:02}:{:02}", s / 60, s % 60);
                    ctx.request_repaint_after(std::time::Duration::from_secs(1));
                }
                ui.label(st);
            });
            if let Some(e) = &error {
                ui.colored_label(Color32::LIGHT_RED, e);
            }
        });

        egui::TopBottomPanel::bottom("hint").resizable(true).min_height(130.0).show(ctx, |ui| {
            ui.add_space(4.0);
            match &hint {
                None => {
                    ui.weak("The hint will appear here (Hint button or H key).");
                }
                Some(h) => {
                    if !h.sources.is_empty() {
                        ui.weak(format!("Sources: {}", h.sources.join(", ")));
                    }
                    if !h.question.is_empty() {
                        ui.label(RichText::new(format!("Question: {}", h.question)).color(Color32::GRAY));
                    }
                    let mut es = h.es.as_str();
                    ui.add(egui::TextEdit::multiline(&mut es).font(egui::FontId::proportional(24.0)).desired_rows(2).desired_width(f32::INFINITY).frame(false));
                    ui.label(RichText::new(&h.en).size(18.0).color(Color32::LIGHT_BLUE));
                    if !h.done {
                        ui.weak("…");
                    }
                }
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
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
                    }
                    ui.add_space(8.0);
                }
            });
        });
    }
}
