use dsp_protocol::{Filter, Variant};
use eframe::egui::{self, Color32, ComboBox, DragValue, Slider, Stroke, Ui, WidgetText};

pub struct FilterWidget {
    filter: Filter,
}

fn variant_text(variant: Variant) -> WidgetText {
    let v: &str = variant.into();
    v.into()
}

impl FilterWidget {
    pub fn from_filter(filter: Filter) -> Self {
        Self { filter }
    }

    pub fn draw(&mut self, ui: &mut Ui) {
        ui.vertical(|ui| {
            egui::Frame::new()
                .stroke(Stroke::new(1.0, Color32::DARK_GRAY))
                .inner_margin(5.0)
                .corner_radius(5.0)
                .show(ui, |ui| {
                    let level = Slider::new(&mut self.filter.level_db, -20.0..=20.0)
                        .vertical()
                        .suffix(" dB");

                    let current_frequency = self.filter.fs_hz;
                    let frequency = DragValue::new(&mut self.filter.fs_hz)
                        .range(20..=20000)
                        .speed(current_frequency / 100.0)
                        .fixed_decimals(0)
                        .update_while_editing(false)
                        .suffix(" Hz");

                    let current_q = self.filter.q_value;
                    let q = DragValue::new(&mut self.filter.q_value)
                        .range(0.01..=20.0)
                        .speed(current_q / 100.0)
                        .max_decimals(2)
                        .update_while_editing(false)
                        .prefix("Q ");

                    ui.add(frequency);
                    ui.add(q);

                    if matches!(
                        self.filter.variant,
                        Variant::LowShelf | Variant::HighShelf | Variant::Peak
                    ) {
                        ui.add(level);
                    }

                    ComboBox::from_id_salt(self.filter.id)
                        .selected_text(variant_text(self.filter.variant))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.filter.variant, Variant::Peak, variant_text(Variant::Peak));
                            ui.selectable_value(
                                &mut self.filter.variant,
                                Variant::LowPass,
                                variant_text(Variant::LowPass),
                            );
                            ui.selectable_value(
                                &mut self.filter.variant,
                                Variant::HighPass,
                                variant_text(Variant::HighPass),
                            );
                            ui.selectable_value(
                                &mut self.filter.variant,
                                Variant::LowShelf,
                                variant_text(Variant::LowShelf),
                            );
                            ui.selectable_value(
                                &mut self.filter.variant,
                                Variant::HighShelf,
                                variant_text(Variant::HighShelf),
                            );
                            ui.selectable_value(
                                &mut self.filter.variant,
                                Variant::AllPass,
                                variant_text(Variant::AllPass),
                            );
                        });

                    ui.checkbox(&mut self.filter.is_muted, "Mute");
                });
        });
    }
}
