use crate::constants::MAX_CYLINDERS;
use crate::utils::{distance_to_samples, samples_to_distance, SPEED_OF_SOUND};
use crate::{audio::Audio, gen::Generator, recorder::Recorder};
use chrono::{Datelike, Local, Timelike};
use crossbeam_channel::Receiver;
use eframe::egui::{self, Color32, TextureHandle, TextureOptions};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::{fs::File, io::Write, sync::Arc};

pub const WATERFALL_WIDTH: u32 = 512;
pub const WATERFALL_HEIGHT: u32 = 50;

pub struct GUIState {
    waterfall: Vec<f32>,
    input: Receiver<Vec<f32>>,
    recording_save_path: Option<PathBuf>,
    config_save_path: Option<PathBuf>,
    config_load_path: Option<PathBuf>,
}

impl GUIState {
    pub fn new(input: Receiver<Vec<f32>>) -> Self {
        GUIState {
            waterfall: vec![0.07f32; (WATERFALL_WIDTH * WATERFALL_HEIGHT) as usize],
            input,
            recording_save_path: None,
            config_save_path: None,
            config_load_path: None,
        }
    }

    fn update(&mut self) -> bool {
        let mut updated = false;
        while let Ok(new_line) = self.input.try_recv() {
            let log_scale = (0..WATERFALL_WIDTH as usize)
                .map(|i| {
                    let new = ((1.0 - (i + 1) as f32 / (WATERFALL_WIDTH + 1) as f32).log2()
                        / (WATERFALL_WIDTH as f32).recip().log2()
                        * (WATERFALL_WIDTH - 1) as f32)
                        .max(1e-3);

                    let idx = new.floor() as usize;
                    new_line[idx.saturating_sub(1)] * (1.0 - new.fract())
                        + new_line[idx] * new.fract()
                })
                .collect::<Vec<f32>>();
            self.add_line(&log_scale);
            updated = true;
        }

        updated
    }

    fn add_line(&mut self, line: &[f32]) {
        assert_eq!(
            line.len(),
            WATERFALL_WIDTH as usize,
            "wrong waterfall line width"
        );

        let row_len = WATERFALL_WIDTH as usize;
        let total_len = (WATERFALL_WIDTH * WATERFALL_HEIGHT) as usize;
        self.waterfall.copy_within(0..(total_len - row_len), row_len);
        self.waterfall[..row_len].copy_from_slice(line);
    }

    fn waterfall_image(&self) -> egui::ColorImage {
        let mut pixels = Vec::with_capacity(self.waterfall.len());
        for value in &self.waterfall {
            let [r, g, b] = waterfall_color(*value);
            pixels.push(Color32::from_rgb(r, g, b));
        }

        egui::ColorImage {
            size: [WATERFALL_WIDTH as usize, WATERFALL_HEIGHT as usize],
            pixels,
        }
    }
}

pub struct EngineSoundApp {
    generator: Arc<RwLock<Generator>>,
    gui_state: GUIState,
    waterfall_texture: Option<TextureHandle>,
    _audio: Audio,
    sample_rate: u32,
    allow_drag_drop: bool,
}

impl EngineSoundApp {
    pub fn new(
        generator: Arc<RwLock<Generator>>,
        input: Receiver<Vec<f32>>,
        audio: Audio,
        sample_rate: u32,
        allow_drag_drop: bool,
    ) -> Self {
        EngineSoundApp {
            generator,
            gui_state: GUIState::new(input),
            waterfall_texture: None,
            _audio: audio,
            sample_rate,
            allow_drag_drop,
        }
    }

    fn update_waterfall_texture(&mut self, ctx: &egui::Context, updated: bool) {
        if !updated && self.waterfall_texture.is_some() {
            return;
        }

        let image = self.gui_state.waterfall_image();
        match &mut self.waterfall_texture {
            Some(texture) => texture.set(image, TextureOptions::NEAREST),
            None => {
                self.waterfall_texture = Some(ctx.load_texture(
                    "waterfall",
                    image,
                    TextureOptions::NEAREST,
                ));
            }
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        if !self.allow_drag_drop {
            return;
        }

        let dropped_files = ctx.input(|input| input.raw.dropped_files.clone());
        if dropped_files.is_empty() {
            return;
        }

        let mut generator = self.generator.write();
        for file in dropped_files {
            let Some(path) = file.path else { continue };
            let Some(path_str) = path.to_str() else { continue };

            match crate::load_engine(
                path_str,
                self.sample_rate,
                path_str.ends_with("json"),
            ) {
                Ok(new_engine) => {
                    println!("Successfully loaded engine config \"{}\"", path_str);
                    generator.engine = new_engine;
                }
                Err(e) => {
                    eprintln!("Failed to load engine config \"{}\": {}", path_str, e);
                }
            }
        }
    }

    fn ui_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let width = ui.available_width();
        let button_height = 20.0;

        if let Some(texture) = &self.waterfall_texture {
            ui.image((texture.id(), egui::vec2(width, 140.0)));
        }

        let mut generator = self.generator.write();
        let sample_rate = generator.samples_per_second;

        let (mut button_label, remove_recorder) = match &mut generator.recorder {
            None => ("Start recording".to_string(), false),
            Some(recorder) => {
                if recorder.is_running() {
                    ctx.request_repaint();
                    (
                        format!(
                            "Stop recording [{:.3} sec recorded]",
                            recorder.get_len() as f32 / sample_rate as f32
                        ),
                        false,
                    )
                } else {
                    ("Start recording".to_string(), true)
                }
            }
        };

        if generator.recording_currently_clipping {
            button_label.push_str("   !!Recording clipping!! (decrease master volume)");
        }

        if remove_recorder {
            generator.recorder = None;
        }

        if ui
            .add_sized(
                [width, button_height],
                egui::Button::new(button_label),
            )
            .clicked()
        {
            match &mut generator.recorder {
                None => {
                    let rec_name = recording_name();
                    let mut dialog = native_dialog::FileDialog::new()
                        .set_filename(&rec_name)
                        .add_filter("MONO Wave Audio file", &["wav"]);

                    if let Some(recording_save_path) = &self.gui_state.recording_save_path {
                        dialog = dialog.set_location(recording_save_path);
                    }

                    if let Some(save_path) = dialog
                        .show_save_single_file()
                        .expect("Failed to open file save dialog")
                    {
                        self.gui_state.recording_save_path =
                            save_path.parent().map(|p| p.to_owned());
                        generator.recorder = Some(Recorder::new(save_path, sample_rate));
                    } else {
                        println!("Aborted recording");
                    }
                }
                Some(recorder) => {
                    recorder.stop();
                }
            }
        }

        if ui
            .add_sized([width, button_height], egui::Button::new("Open file"))
            .clicked()
        {
            let mut dialog = native_dialog::FileDialog::new()
                .add_filter("Engine sound configuration files", &["esc", "es"])
                .add_filter("All files", &["*"]);

            if let Some(config_load_path) = &self.gui_state.config_load_path {
                dialog = dialog.set_location(config_load_path);
            }

            let load_file_path = dialog.show_open_single_file().unwrap();

            if let Some(load_file_path) = load_file_path {
                self.gui_state.config_load_path = load_file_path.parent().map(|p| p.to_owned());

                let string_path = load_file_path.display().to_string();

                match crate::load_engine(
                    &string_path,
                    sample_rate,
                    string_path.ends_with("json"),
                ) {
                    Ok(new_engine) => {
                        println!("Successfully loaded engine config \"{}\"", &string_path);
                        generator.engine = new_engine;
                    }
                    Err(e) => {
                        eprintln!("Failed to load engine config \"{}\": {}", &string_path, e);
                    }
                }
            } else {
                println!("Cancelled file loading dialog");
            }
        }

        let mut reset_sampler_label = String::from("Panic!");

        if generator.waveguides_dampened {
            reset_sampler_label.push_str("   !!Resonances dampened!! (change parameters)");
        }

        if ui
            .add_sized(
                [width, button_height * 3.0],
                egui::Button::new(reset_sampler_label).fill(Color32::from_rgb(204, 25, 25)),
            )
            .clicked()
        {
            generator.volume = generator.volume.min(0.01);
            generator.reset();
        }

        if ui
            .add_sized([width, button_height], egui::Button::new("Save"))
            .clicked()
        {
            let pretty = ron::ser::PrettyConfig::new()
                .with_separate_tuple_members(true)
                .with_enumerate_arrays(true);

            let name = config_name();

            let mut dialog = native_dialog::FileDialog::new()
                .set_filename(&name)
                .add_filter("Engine sound RON file", &["esc", "ron"])
                .add_filter("Engine sound JSON file", &["json"]);

            if let Some(config_save_path) = &self.gui_state.config_save_path {
                dialog = dialog.set_location(config_save_path);
            }

            if let Some(path) = dialog
                .show_save_single_file()
                .expect("Failed to open file save dialog")
            {
                self.gui_state.config_save_path = path.parent().map(|p| p.to_owned());

                match path.extension() {
                    Some(ext) if ext == "json" => {
                        match serde_json::to_string_pretty(&generator.engine) {
                            Ok(s) => match File::create(&path) {
                                Ok(mut file) => {
                                    file.write_all(s.as_bytes()).unwrap();

                                    println!(
                                        "Successfully saved engine config \"{}\"",
                                        &path.display()
                                    );
                                }
                                Err(e) => {
                                    eprintln!(
                                        "Failed to create file for saving engine config: {}",
                                        e
                                    )
                                }
                            },
                            Err(e) => eprintln!("Failed to save engine config: {}", e),
                        }
                    }
                    _ => match ron::ser::to_string_pretty(&generator.engine, pretty) {
                        Ok(s) => match File::create(&path) {
                            Ok(mut file) => {
                                file.write_all(s.as_bytes()).unwrap();

                                println!(
                                    "Successfully saved engine config \"{}\"",
                                    &path.display()
                                );
                            }
                            Err(e) => {
                                eprintln!("Failed to create file for saving engine config: {}", e)
                            }
                        },
                        Err(e) => eprintln!("Failed to save engine config: {}", e),
                    },
                }
            } else {
                println!("Cancelled saving");
            }
        }

        ui.add_space(6.0);
        ui.heading("Mix");

        let prev_val = generator.engine.rpm;
        let mut rpm = prev_val;
        if ui
            .add(
                egui::Slider::new(&mut rpm, 300.0..=13000.0)
                    .text(format!(
                        "Engine RPM {:.2} ({:.1} hz)",
                        prev_val,
                        prev_val / 60.0
                    )),
            )
            .changed()
        {
            generator.engine.rpm = rpm;
        }

        {
            let mut volume = generator.volume;
            let label = format!("Master volume {:.0}%", volume * 100.0);
            if ui
                .add(
                    egui::Slider::new(&mut volume, 0.0..=3.0).text(label),
                )
                .changed()
            {
                generator.volume = volume;
            }
        }

        {
            let prev_val = generator.engine.intake_volume;
            let mut value = prev_val;
            if ui
                .add(
                    egui::Slider::new(&mut value, 0.0..=1.0)
                        .text(format!("Intake volume {:.0}%", prev_val * 100.0)),
                )
                .changed()
            {
                let mut dif = value - prev_val;
                generator.engine.intake_volume = value;
                let v1 = generator.engine.exhaust_volume;
                let v2 = generator.engine.engine_vibrations_volume;
                if v1 < v2 {
                    let vv1 = v1.min(dif * 0.5);
                    dif -= vv1;
                    generator.engine.exhaust_volume = (v1 - vv1).min(1.0).max(0.0);
                    generator.engine.engine_vibrations_volume = (v2 - dif).min(1.0).max(0.0);
                } else {
                    let vv2 = v2.min(dif * 0.5);
                    dif -= vv2;
                    generator.engine.engine_vibrations_volume = (v2 - vv2).min(1.0).max(0.0);
                    generator.engine.exhaust_volume = (v1 - dif).min(1.0).max(0.0);
                }
            }
        }

        {
            let prev_val = generator.engine.exhaust_volume;
            let mut value = prev_val;
            if ui
                .add(
                    egui::Slider::new(&mut value, 0.0..=1.0)
                        .text(format!("Exhaust volume {:.0}%", prev_val * 100.0)),
                )
                .changed()
            {
                let mut dif = value - prev_val;
                generator.engine.exhaust_volume = value;
                let v1 = generator.engine.intake_volume;
                let v2 = generator.engine.engine_vibrations_volume;
                if v1 < v2 {
                    let vv1 = v1.min(dif * 0.5);
                    dif -= vv1;
                    generator.engine.intake_volume = (v1 - vv1).min(1.0).max(0.0);
                    generator.engine.engine_vibrations_volume = (v2 - dif).min(1.0).max(0.0);
                } else {
                    let vv2 = v2.min(dif * 0.5);
                    dif -= vv2;
                    generator.engine.engine_vibrations_volume = (v2 - vv2).min(1.0).max(0.0);
                    generator.engine.intake_volume = (v1 - dif).min(1.0).max(0.0);
                }
            }
        }

        {
            let prev_val = generator.engine.engine_vibrations_volume;
            let mut value = prev_val;
            let label =
                format!("Engine vibrations volume {:.0}%", prev_val * 100.0);
            if ui
                .add(
                    egui::Slider::new(&mut value, 0.0..=1.0).text(label),
                )
                .changed()
            {
                let mut dif = value - prev_val;
                generator.engine.engine_vibrations_volume = value;
                let v1 = generator.engine.exhaust_volume;
                let v2 = generator.engine.intake_volume;
                if v1 < v2 {
                    let vv1 = v1.min(dif * 0.5);
                    dif -= vv1;
                    generator.engine.exhaust_volume = (v1 - vv1).min(1.0).max(0.0);
                    generator.engine.intake_volume = (v2 - dif).min(1.0).max(0.0);
                } else {
                    let vv2 = v2.min(dif * 0.5);
                    dif -= vv2;
                    generator.engine.intake_volume = (v2 - vv2).min(1.0).max(0.0);
                    generator.engine.exhaust_volume = (v1 - dif).min(1.0).max(0.0);
                }
            }
        }

        {
            let iv = generator.engine.intake_volume;
            let ev = generator.engine.exhaust_volume;
            let evv = generator.engine.engine_vibrations_volume;
            let sum = iv + ev + evv;
            generator.engine.intake_volume = iv / sum;
            generator.engine.exhaust_volume = ev / sum;
            generator.engine.engine_vibrations_volume = evv / sum;
        }

        ui.add_space(6.0);
        ui.heading("Engine parameters");

        {
            let prev_val = generator.engine.engine_vibration_filter.get_freq();
            let max = sample_rate as f32 * 0.5;
            let mut value = prev_val;
            if ui
                .add(
                    egui::Slider::new(&mut value, 10.0..=max).text(format!(
                        "Engine vibrations Lowpass-Filter Frequency {:.2}hz",
                        prev_val
                    )),
                )
                .changed()
            {
                if let Some(new) = generator
                    .engine
                    .engine_vibration_filter
                    .get_changed(value, sample_rate)
                {
                    generator.engine.engine_vibration_filter = new;
                }
            }
        }

        {
            let mut value = generator.engine.intake_noise_factor;
            let label = format!("Intake noise volume {:.2}", value);
            if ui
                .add(
                    egui::Slider::new(&mut value, 0.0..=3.0).text(label),
                )
                .changed()
            {
                generator.engine.intake_noise_factor = value;
            }
        }

        {
            let prev_val = generator.engine.intake_noise_lp.get_freq();
            let max = sample_rate as f32 * 0.5;
            let mut value = prev_val;
            if ui
                .add(
                    egui::Slider::new(&mut value, 10.0..=max).text(format!(
                        "Intake noise Lowpass-Filter Frequency {:.2}hz",
                        prev_val
                    )),
                )
                .changed()
            {
                if let Some(new) = generator
                    .engine
                    .intake_noise_lp
                    .get_changed(value, sample_rate)
                {
                    generator.engine.intake_noise_lp = new;
                }
            }
        }

        {
            let mut value = generator.engine.intake_valve_shift;
            let label = format!("Intake valve cam shift {:.2} cycles", -value);
            if ui
                .add(
                    egui::Slider::new(&mut value, -0.5..=0.5).text(label),
                )
                .changed()
            {
                generator.engine.intake_valve_shift = value;
            }
        }

        {
            let mut value = generator.engine.exhaust_valve_shift;
            let label = format!("Exhaust valve cam shift {:.2} cycles", -value);
            if ui
                .add(
                    egui::Slider::new(&mut value, -0.5..=0.5).text(label),
                )
                .changed()
            {
                generator.engine.exhaust_valve_shift = value;
            }
        }

        {
            let mut value = generator.engine.crankshaft_fluctuation;
            let label = format!("Crankshaft fluctuation factor {:.2}x", value);
            if ui
                .add(
                    egui::Slider::new(&mut value, 0.0..=2.5).text(label),
                )
                .changed()
            {
                generator.engine.crankshaft_fluctuation = value;
            }
        }

        {
            let prev_val = generator.engine.crankshaft_fluctuation_lp.get_freq();
            let max = sample_rate as f32 * 0.5;
            let mut value = prev_val;
            if ui
                .add(
                    egui::Slider::new(&mut value, 10.0..=max).text(format!(
                        "Crankshaft fluctuation noise Lowpass-Filter frequency {:.2}hz",
                        prev_val
                    )),
                )
                .changed()
            {
                if let Some(new) = generator
                    .engine
                    .crankshaft_fluctuation_lp
                    .get_changed(value, sample_rate)
                {
                    generator.engine.crankshaft_fluctuation_lp = new;
                }
            }
        }

        ui.add_space(6.0);
        ui.heading("Muffler parameters");

        {
            let mut value = generator.engine.muffler.straight_pipe.alpha;
            let label = format!(
                "Straight Pipe extractor-side reflectivity {:.2}",
                value
            );
            if ui
                .add(
                    egui::Slider::new(&mut value, -1.0..=1.0).text(label),
                )
                .changed()
            {
                generator.engine.muffler.straight_pipe.alpha = value;
            }
        }

        {
            let mut value = generator.engine.muffler.straight_pipe.beta;
            let label = format!(
                "Straight Pipe muffler-side reflectivity {:.2}",
                value
            );
            if ui
                .add(
                    egui::Slider::new(&mut value, -1.0..=1.0).text(label),
                )
                .changed()
            {
                generator.engine.muffler.straight_pipe.beta = value;
            }
        }

        {
            let prev_val = generator.engine.muffler.straight_pipe.chamber0.samples.data.len() as f32
                * SPEED_OF_SOUND
                / sample_rate as f32;
            let mut value = prev_val;
            if ui
                .add(
                    egui::Slider::new(&mut value, 0.1..=3.0).text(format!(
                        "Straight Pipe length {:.2}m ({:.1}hz sine peak)",
                        prev_val,
                        SPEED_OF_SOUND / prev_val * 2.0
                    )),
                )
                .changed()
            {
                let alpha = generator.engine.muffler.straight_pipe.alpha;
                let beta = generator.engine.muffler.straight_pipe.beta;

                if let Some(newgen) = generator.engine.muffler.straight_pipe.get_changed(
                    (value / SPEED_OF_SOUND * sample_rate as f32) as usize,
                    alpha,
                    beta,
                    sample_rate,
                ) {
                    generator.engine.muffler.straight_pipe = newgen;
                }
            }
        }

        let mut muffler_elements_beta = generator.engine.muffler.muffler_elements[0].beta;
        {
            let mut value = muffler_elements_beta;
            let label = format!(
                "Muffler elements output-side (exhaust) reflectivity {:.2}x",
                value
            );
            if ui
                .add(
                    egui::Slider::new(&mut value, -1.0..=0.3).text(label),
                )
                .changed()
            {
                muffler_elements_beta = value;
            }
        }

        for (i, muffler_element) in generator
            .engine
            .muffler
            .muffler_elements
            .iter_mut()
            .enumerate()
        {
            let prev_val = samples_to_distance(
                muffler_element.chamber0.samples.data.len(),
                sample_rate,
            );
            let mut value = prev_val;
            if ui
                .add(
                    egui::Slider::new(&mut value, 0.001..=0.6).text(format!(
                        "{} / Muffler cavity length {:.2}m ({:.1}hz sine peak)",
                        i + 1,
                        prev_val,
                        SPEED_OF_SOUND / prev_val * 2.0
                    )),
                )
                .changed()
            {
                let new = muffler_element.get_changed(
                    distance_to_samples(value, sample_rate),
                    muffler_element.alpha,
                    muffler_element.beta,
                    sample_rate,
                );

                if let Some(new) = new {
                    muffler_element.clone_from(&new);
                }
            }
            muffler_element.beta = muffler_elements_beta;
        }

        ui.add_space(6.0);
        ui.heading("Cylinder parameters");

        let mut changed = false;
        let mut num_cylinders = generator.engine.cylinders.len();

        {
            let mut value = num_cylinders as f32;
            if ui
                .add(egui::Slider::new(&mut value, 1.0..=MAX_CYLINDERS as f32).text(
                    format!("Cylinder count {}", num_cylinders),
                ))
                .changed()
            {
                let value = value.round() as usize;
                if value != num_cylinders {
                    changed = true;
                    num_cylinders = value;
                }
            }
        }

        let mut cylinder = generator.engine.cylinders[0].clone();

        {
            let mut value = cylinder.intake_open_refl;
            let label = format!(
                "Opened intake valve intake-cavity reflectivity {:.2}",
                value
            );
            if ui
                .add(
                    egui::Slider::new(&mut value, -1.0..=1.0).text(label),
                )
                .changed()
            {
                changed = true;
                cylinder.intake_open_refl = value;
            }
        }

        {
            let mut value = cylinder.intake_closed_refl;
            let label = format!(
                "Closed intake valve intake-cavity reflectivity {:.2}",
                value
            );
            if ui
                .add(
                    egui::Slider::new(&mut value, -1.0..=1.0).text(label),
                )
                .changed()
            {
                changed = true;
                cylinder.intake_closed_refl = value;
            }
        }

        {
            let mut value = cylinder.exhaust_open_refl;
            let label = format!(
                "Opened exhaust valve exhaust-cavity reflectivity {:.2}",
                value
            );
            if ui
                .add(
                    egui::Slider::new(&mut value, -1.0..=1.0).text(label),
                )
                .changed()
            {
                changed = true;
                cylinder.exhaust_open_refl = value;
            }
        }

        {
            let mut value = cylinder.exhaust_closed_refl;
            let label = format!(
                "Closed exhaust valve exhaust-cavity reflectivity {:.2}",
                value
            );
            if ui
                .add(
                    egui::Slider::new(&mut value, -1.0..=1.0).text(label),
                )
                .changed()
            {
                changed = true;
                cylinder.exhaust_closed_refl = value;
            }
        }

        {
            let mut value = cylinder.intake_waveguide.beta;
            let label = format!(
                "Intake-cavity open end reflectivity {:.2}",
                value
            );
            if ui
                .add(
                    egui::Slider::new(&mut value, -1.0..=1.0).text(label),
                )
                .changed()
            {
                changed = true;
                cylinder.intake_waveguide.beta = value;
            }
        }

        {
            let mut value = cylinder.extractor_waveguide.beta;
            let label = format!(
                "Extractor-cavity straight pipe side reflectivity {:.2}",
                value
            );
            if ui
                .add(
                    egui::Slider::new(&mut value, -1.0..=1.0).text(label),
                )
                .changed()
            {
                changed = true;
                cylinder.extractor_waveguide.beta = value;
            }
        }

        {
            let mut value = cylinder.piston_motion_factor;
            let label = format!("Piston motion volume {:.2}", value);
            if ui
                .add(
                    egui::Slider::new(&mut value, 0.0..=20.0).text(label),
                )
                .changed()
            {
                changed = true;
                cylinder.piston_motion_factor = value;
            }
        }

        {
            let mut value = cylinder.ignition_factor;
            let label = format!("Ignition volume {:.2}", value);
            if ui
                .add(
                    egui::Slider::new(&mut value, 0.0..=20.0).text(label),
                )
                .changed()
            {
                changed = true;
                cylinder.ignition_factor = value;
            }
        }

        {
            let mut value = cylinder.ignition_time;
            let label = format!("Ignition time {:.2}", value);
            if ui
                .add(
                    egui::Slider::new(&mut value, 0.0..=0.3).text(label),
                )
                .changed()
            {
                changed = true;
                cylinder.ignition_time = value;
            }
        }

        if changed {
            generator.engine.cylinders = if num_cylinders <= generator.engine.cylinders.len() {
                let mut new_cylinders = generator.engine.cylinders[0..num_cylinders].to_vec();

                for cyl in new_cylinders.iter_mut() {
                    cyl.intake_open_refl = cylinder.intake_open_refl;
                    cyl.intake_closed_refl = cylinder.intake_closed_refl;
                    cyl.exhaust_open_refl = cylinder.exhaust_open_refl;
                    cyl.exhaust_closed_refl = cylinder.exhaust_closed_refl;
                    cyl.piston_motion_factor = cylinder.piston_motion_factor;
                    cyl.ignition_factor = cylinder.ignition_factor;
                    cyl.ignition_time = cylinder.ignition_time;
                    cyl.intake_waveguide.beta = cylinder.intake_waveguide.beta;
                    cyl.extractor_waveguide.beta = cylinder.extractor_waveguide.beta;
                }

                new_cylinders
            } else {
                let mut new_cylinders = generator.engine.cylinders.to_vec();

                for cyl in new_cylinders.iter_mut() {
                    cyl.intake_open_refl = cylinder.intake_open_refl;
                    cyl.intake_closed_refl = cylinder.intake_closed_refl;
                    cyl.exhaust_open_refl = cylinder.exhaust_open_refl;
                    cyl.exhaust_closed_refl = cylinder.exhaust_closed_refl;
                    cyl.piston_motion_factor = cylinder.piston_motion_factor;
                    cyl.ignition_factor = cylinder.ignition_factor;
                    cyl.ignition_time = cylinder.ignition_time;
                    cyl.intake_waveguide.beta = cylinder.intake_waveguide.beta;
                    cyl.extractor_waveguide.beta = cylinder.extractor_waveguide.beta;
                }

                for _ in generator.engine.cylinders.len()..num_cylinders {
                    cylinder.crank_offset = (num_cylinders - 1) as f32 / num_cylinders as f32;
                    new_cylinders.push(cylinder.clone());
                }

                new_cylinders
            };
        }

        for (i, cyl) in generator.engine.cylinders.iter_mut().enumerate() {
            let prev_val = samples_to_distance(
                cyl.intake_waveguide.chamber0.samples.data.len(),
                sample_rate,
            );
            let mut value = prev_val;
            let label = format!(
                "{} / Intake-cavity length {:.2}m",
                i + 1,
                prev_val
            );
            if ui
                .add(egui::Slider::new(&mut value, 0.0..=1.0).text(label))
                .changed()
            {
                let new = cyl.intake_waveguide.get_changed(
                    distance_to_samples(value, sample_rate),
                    cyl.intake_waveguide.alpha,
                    cyl.intake_waveguide.beta,
                    sample_rate,
                );

                if let Some(new) = new {
                    cyl.intake_waveguide = new;
                }
            }

            let prev_val = samples_to_distance(
                cyl.exhaust_waveguide.chamber0.samples.data.len(),
                sample_rate,
            );
            let mut value = prev_val;
            let label = format!(
                "{} / Exhaust-cavity length {:.2}m",
                i + 1,
                prev_val
            );
            if ui
                .add(egui::Slider::new(&mut value, 0.0..=1.7).text(label))
                .changed()
            {
                let new = cyl.exhaust_waveguide.get_changed(
                    distance_to_samples(value, sample_rate),
                    cyl.exhaust_waveguide.alpha,
                    cyl.exhaust_waveguide.beta,
                    sample_rate,
                );

                if let Some(new) = new {
                    cyl.exhaust_waveguide = new;
                }
            }

            let prev_val = samples_to_distance(
                cyl.extractor_waveguide.chamber0.samples.data.len(),
                sample_rate,
            );
            let mut value = prev_val;
            let label = format!(
                "{} / Extractor-cavity length {:.2}m",
                i + 1,
                prev_val
            );
            if ui
                .add(egui::Slider::new(&mut value, 0.0..=10.0).text(label))
                .changed()
            {
                let new = cyl.extractor_waveguide.get_changed(
                    distance_to_samples(value, sample_rate),
                    cyl.extractor_waveguide.alpha,
                    cyl.extractor_waveguide.beta,
                    sample_rate,
                );

                if let Some(new) = new {
                    cyl.extractor_waveguide = new;
                }
            }

            let mut value = cyl.crank_offset;
            let label = format!(
                "{} / Crank offset {:.3} cycles",
                i + 1,
                value
            );
            if ui
                .add(egui::Slider::new(&mut value, 0.0..=1.0).text(label))
                .changed()
            {
                cyl.crank_offset = value;
            }
        }
    }
}

impl eframe::App for EngineSoundApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let updated = self.gui_state.update();
        self.update_waterfall_texture(ctx, updated);
        self.handle_dropped_files(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.ui_controls(ui, ctx);
            });
        });
    }
}

fn waterfall_color(x: f32) -> [u8; 3] {
    fn mix(x: f32, colors: &[([f32; 3], f32)]) -> [f32; 3] {
        let colors = colors
            .windows(2)
            .find(|colors| {
                let (_, start) = colors[0];
                let (_, end) = colors[1];
                start <= x && x < end
            })
            .expect("invalid color mix range");

        let (low_color, low) = colors[0];
        let (high_color, high) = colors[1];

        let ratio = (x - low) / (high - low);
        [
            low_color[0] + (high_color[0] - low_color[0]) * ratio,
            low_color[1] + (high_color[1] - low_color[1]) * ratio,
            low_color[2] + (high_color[2] - low_color[2]) * ratio,
        ]
    }

    let color = mix(
        x.max(0.0).min(10.0),
        &[
            ([0.0, 0.0, 0.0], 0.0),
            ([0.0, 0.2, 0.23], 0.21),
            ([0.0, 0.3, 0.6], 0.325),
            ([0.51, 0.36, 1.0], 0.44),
            ([1.0, 0.55, 0.0], 0.69),
            ([1.0, 0.86, 0.69], 0.85),
            ([1.0, 1.0, 1.0], 1.0),
            ([1.0, 1.0, 1.0], 10.01),
        ],
    );

    [
        (color[0].max(0.0).min(1.0) * 255.0) as u8,
        (color[1].max(0.0).min(1.0) * 255.0) as u8,
        (color[2].max(0.0).min(1.0) * 255.0) as u8,
    ]
}

fn recording_name() -> String {
    let time = Local::now();

    format!(
        "enginesound_{:02}{:02}{:04}-{:02}{:02}{:02}.wav",
        time.day(),
        time.month(),
        time.year(),
        time.hour(),
        time.minute(),
        time.second()
    )
}

fn config_name() -> String {
    let time = Local::now();

    format!(
        "enginesound_{:02}{:02}{:04}-{:02}{:02}{:02}.esc",
        time.day(),
        time.month(),
        time.year(),
        time.hour(),
        time.minute(),
        time.second()
    )
}
