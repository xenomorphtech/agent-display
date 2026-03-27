use chrono::{DateTime, Utc};
use eframe::egui;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Item {
    id: String,
    title: String,
    content: String,
    content_type: ContentType,
    source: String,
    timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ContentType {
    Markdown,
    Html,
}

struct ViewerApp {
    items: Arc<Mutex<Vec<Item>>>,
    selected_id: Option<String>,
    new_item_flash: f32,
    connected: Arc<Mutex<bool>>,
    commonmark_cache: egui_commonmark::CommonMarkCache,
}

impl ViewerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let items: Arc<Mutex<Vec<Item>>> = Arc::new(Mutex::new(Vec::new()));
        let connected: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let ctx = cc.egui_ctx.clone();

        // Fetch existing items on startup
        let items_clone = items.clone();
        let ctx_clone = ctx.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                if let Ok(resp) = reqwest::get("http://127.0.0.1:3080/items").await {
                    if let Ok(fetched) = resp.json::<Vec<Item>>().await {
                        let mut lock = items_clone.lock().unwrap();
                        *lock = fetched;
                        lock.reverse(); // Server returns newest first, we store oldest first
                        ctx_clone.request_repaint();
                    }
                }
            });
        });

        // WebSocket connection in background thread
        let items_ws = items.clone();
        let connected_ws = connected.clone();
        let ctx_ws = ctx.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                loop {
                    let _ = connect_ws(&items_ws, &connected_ws, &ctx_ws).await;
                    *connected_ws.lock().unwrap() = false;
                    ctx_ws.request_repaint();
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            });
        });

        Self {
            items,
            selected_id: None,
            new_item_flash: 0.0,
            connected,
            commonmark_cache: egui_commonmark::CommonMarkCache::default(),
        }
    }
}

async fn connect_ws(
    items: &Arc<Mutex<Vec<Item>>>,
    connected: &Arc<Mutex<bool>>,
    ctx: &egui::Context,
) -> Result<(), Box<dyn std::error::Error>> {
    let (ws_stream, _) =
        tokio_tungstenite::connect_async("ws://127.0.0.1:3080/ws").await?;

    *connected.lock().unwrap() = true;
    ctx.request_repaint();

    let (_, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        match msg {
            Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                if let Ok(item) = serde_json::from_str::<Item>(&text) {
                    let mut lock = items.lock().unwrap();
                    lock.push(item);
                    ctx.request_repaint();
                }
            }
            Err(_) => break,
            _ => {}
        }
    }

    Ok(())
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Decay the flash effect
        if self.new_item_flash > 0.0 {
            self.new_item_flash -= 0.02;
            ctx.request_repaint();
        }

        let items = self.items.lock().unwrap().clone();
        let is_connected = *self.connected.lock().unwrap();

        // Auto-select newest item if a new one arrived
        if let Some(newest) = items.last() {
            if self.selected_id.as_ref() != Some(&newest.id) {
                let was_some = self.selected_id.is_some();
                self.selected_id = Some(newest.id.clone());
                if was_some {
                    self.new_item_flash = 1.0;
                }
            }
        }

        // Top bar
        egui::Panel::top("top_panel").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("LLM Viewer");
                ui.separator();
                let status_color = if is_connected {
                    egui::Color32::from_rgb(80, 200, 80)
                } else {
                    egui::Color32::from_rgb(200, 80, 80)
                };
                let status_text = if is_connected {
                    "Connected"
                } else {
                    "Disconnected"
                };
                ui.colored_label(status_color, format!("● {}", status_text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("{} items", items.len()));
                });
            });
        });

        // Sidebar
        egui::Panel::left("sidebar")
            .default_size(280.0)
            .min_size(200.0)
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Recent Items");
                ui.separator();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Show newest first in sidebar
                    for item in items.iter().rev() {
                        let is_selected = self.selected_id.as_ref() == Some(&item.id);

                        let frame = if is_selected {
                            egui::Frame::NONE
                                .fill(egui::Color32::from_rgba_premultiplied(60, 80, 120, 255))
                                .inner_margin(8)
                                .corner_radius(4)
                        } else {
                            egui::Frame::NONE
                                .inner_margin(8)
                                .corner_radius(4)
                        };

                        let response = frame
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());

                                ui.horizontal(|ui| {
                                    // Content type badge
                                    let (badge_text, badge_color) = match item.content_type {
                                        ContentType::Markdown => {
                                            ("MD", egui::Color32::from_rgb(100, 180, 255))
                                        }
                                        ContentType::Html => {
                                            ("HTML", egui::Color32::from_rgb(255, 150, 80))
                                        }
                                    };

                                    let badge_frame = egui::Frame::NONE
                                        .fill(badge_color.gamma_multiply(0.3))
                                        .inner_margin(egui::Margin::symmetric(6, 2))
                                        .corner_radius(3);

                                    badge_frame.show(ui, |ui| {
                                        ui.label(
                                            egui::RichText::new(badge_text)
                                                .color(badge_color)
                                                .small()
                                                .strong(),
                                        );
                                    });

                                    ui.label(
                                        egui::RichText::new(&item.title)
                                            .strong()
                                            .color(egui::Color32::WHITE),
                                    );
                                });

                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(&item.source)
                                            .small()
                                            .color(egui::Color32::from_rgb(150, 150, 180)),
                                    );
                                    ui.label(
                                        egui::RichText::new(
                                            item.timestamp.format("%H:%M:%S").to_string(),
                                        )
                                        .small()
                                        .color(egui::Color32::from_rgb(120, 120, 140)),
                                    );
                                });
                            })
                            .response;

                        if response.interact(egui::Sense::click()).clicked() {
                            self.selected_id = Some(item.id.clone());
                        }

                        if response.interact(egui::Sense::hover()).hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }

                        ui.add_space(2.0);
                    }
                });
            });

        // Main content area
        egui::CentralPanel::default().show_inside(ui, |ui| {
            if let Some(selected_id) = &self.selected_id {
                if let Some(item) = items.iter().find(|i| &i.id == selected_id) {
                    // Header
                    ui.horizontal(|ui| {
                        ui.heading(&item.title);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new(
                                        item.timestamp.format("%Y-%m-%d %H:%M:%S").to_string(),
                                    )
                                    .small()
                                    .color(egui::Color32::GRAY),
                                );
                                ui.label(
                                    egui::RichText::new(format!("from {}", item.source))
                                        .small()
                                        .color(egui::Color32::from_rgb(150, 150, 180)),
                                );
                            },
                        );
                    });
                    ui.separator();

                    // Content
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.add_space(8.0);
                        match item.content_type {
                            ContentType::Markdown => {
                                egui_commonmark::CommonMarkViewer::new()
                                    .show(ui, &mut self.commonmark_cache, &item.content);
                            }
                            ContentType::Html => {
                                // For v1, render HTML as monospace text
                                ui.label(
                                    egui::RichText::new(&item.content)
                                        .monospace()
                                        .color(egui::Color32::from_rgb(220, 220, 220)),
                                );
                            }
                        }
                        ui.add_space(16.0);
                    });
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("Item not found")
                                .color(egui::Color32::GRAY)
                                .size(18.0),
                        );
                    });
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("No items yet — waiting for content...")
                            .color(egui::Color32::GRAY)
                            .size(18.0),
                    );
                });
            }
        });
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("LLM Viewer"),
        ..Default::default()
    };

    eframe::run_native(
        "LLM Viewer",
        options,
        Box::new(|cc| Ok(Box::new(ViewerApp::new(cc)))),
    )
}
