#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use std::time::Duration;

use dsp_gui::widgets::FilterWidget;
use dsp_gui::Client;
use dsp_protocol::{Channel, Filter};
use eframe::egui::{self, ScrollArea};
use log::{info, warn};

enum Command {
    FetchInfo,
}

#[tokio::main]
async fn main() -> eframe::Result {
    colog::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

    tokio::task::spawn(client_task());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default(),
        ..Default::default()
    };

    let mut dsps1 = vec![
        FilterWidget::from_filter(Filter::new(0)),
        FilterWidget::from_filter(Filter::new(1)),
        FilterWidget::from_filter(Filter::new(2)),
    ];
    let mut dsps2 = vec![
        FilterWidget::from_filter(Filter::new(5)),
        FilterWidget::from_filter(Filter::new(6)),
        FilterWidget::from_filter(Filter::new(7)),
    ];

    eframe::run_simple_native("My egui App", options, move |ctx, _frame| {
        egui::CentralPanel::default().show(ctx, |ui| {
            ScrollArea::both().show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.heading("Channel 1");
                    ui.horizontal(|ui| {
                        for dsp in dsps1.iter_mut() {
                            dsp.draw(ui);
                        }
                    });

                    ui.separator();

                    ui.heading("Channel 2");
                    ui.horizontal(|ui| {
                        for dsp in dsps2.iter_mut() {
                            dsp.draw(ui);
                        }
                    });
                });
            });
        });
    })
}

async fn client_task() {
    loop {
        let Ok(client) = Client::try_new() else {
            tokio::time::sleep(Duration::from_secs(1)).await;
            info!("Retry connection...");
            continue;
        };

        tokio::select! {
            _ = client.wait_closed() => {
                warn!("Client is closed, exiting...");
            }
            _ = run(&client) => {
                info!("App is done")
            }
        }
    }
}

async fn run(client: &Client) {
    // Fetch device info before proceeding.
    let info = client.info().await.unwrap();
    let channel_count = info.channel_count;

    for channel_id in 0..channel_count {
        let filters: Vec<Filter> = (0..10)
            .map(|id| Filter {
                id,
                ..Default::default()
            })
            .collect();
        let filters = filters.try_into().unwrap();

        let channel = Channel {
            id: channel_id,
            input_id: 0,
            filters,
        };
        client.set_channel(channel).await.unwrap();
    }
}
