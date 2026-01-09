use crate::exactstreamer::ExactStreamer;
use crate::gen::LowPassFilter;
use crate::recorder::Recorder;
use crate::utils::{fix_engine, load_engine, seconds_to_samples};
use clap::{value_t, value_t_or_exit, App, Arg};
use parking_lot::RwLock;
use std::sync::Arc;

#[cfg(feature = "gui")]
use crate::{audio::GENERATOR_BUFFER_SIZE, fft::FFTStreamer, gui::WATERFALL_WIDTH};
#[cfg(feature = "gui")]
use eframe::egui;

#[cfg(feature = "gui")]
mod audio;
#[cfg(feature = "gui")]
mod fft;
#[cfg(feature = "gui")]
mod gui;

mod constants;
mod exactstreamer;
mod gen;
mod recorder;
mod utils;

#[cfg(feature = "gui")]
const WINDOW_WIDTH: f32 = 800.0;
#[cfg(feature = "gui")]
const WINDOW_HEIGHT: f32 = 800.0;

const DEFAULT_CONFIG: &[u8] = include_bytes!("default.esc");

fn main() {
    let matches = App::new("Engine Sound Generator")
        .version(clap::crate_version!())
        .author(clap::crate_authors!())
        .about(clap::crate_description!())
        .arg(Arg::with_name("headless").short("h").long("headless").help("CLI mode without GUI or audio playback").requires("config"))
        .arg(Arg::with_name("config").short("c").long("config").help("Sets the input file to load as an engine config").takes_value(true))
        .arg(Arg::with_name("volume").short("v").long("volume").help("Sets the master volume").default_value( "0.1"))
        .arg(Arg::with_name("rpm").short("r").long("rpm").help("Engine RPM").takes_value(true))
        .arg(Arg::with_name("warmup_time").short("w").long("warmup_time").help("Sets the time to wait in seconds before recording").default_value_if("headless", None, "3.0"))
        .arg(Arg::with_name("reclen").short("l").long("length").help("Sets the time to record in seconds. The formula for the recommended time to record to get a seamless loop is as follows:\nlet wavelength = 120.0 / rpm;\nlet crossfade = wavelength * 2.0;\nlet reclen = audio_length + crossfade / 2.0;").default_value_if("headless", None, "5.0"))
        .arg(Arg::with_name("output_file").short("o").long("output").help("Sets the output .wav file path").default_value_if("headless", None, "output.wav"))
        .arg(Arg::with_name("crossfade").short("f").long("crossfade").help("Crossfades the recording in the middle end-to-start to create a seamless loop, although adjusting the recording's length to the rpm is recommended. The value sets the size of the crossfade, where the final output is decreased in length by crossfade_time/2.").default_value_if("headless", None, "0.00133"))
        .arg(Arg::with_name("samplerate").short("q").long("samplerate").help("Generator sample rate").default_value("48000"))
        .arg(Arg::with_name("no-drag-drop").short("d").long("no-drag-drop").help("Disabled drag-and-drop support for the window").conflicts_with("headless"))
        .get_matches();

    let sample_rate = value_t_or_exit!(matches, "samplerate", u32);

    let mut engine = match matches.value_of("config") {
        Some(path) => match load_engine(path, sample_rate, path.ends_with("json")) {
            Ok(engine) => {
                println!("Successfully loaded config \"{}\"", path);
                engine
            }
            Err(e) => {
                eprintln!("Failed to load engine config \"{}\": {}", path, e);
                std::process::exit(1);
            }
        },
        None => {
            let mut engine =
                ron::de::from_bytes(DEFAULT_CONFIG).expect("default config is invalid");
            fix_engine(&mut engine, sample_rate);
            engine
        }
    };

    if let Ok(rpm) = value_t!(matches, "rpm", f32) {
        engine.rpm = rpm.max(0.0);
    }

    let cli_mode = matches.is_present("headless");

    // sound generator
    let mut generator =
        gen::Generator::new(sample_rate, engine, LowPassFilter::new(0.5, sample_rate));

    generator.volume = value_t!(matches.value_of("volume"), f32).unwrap();

    if cli_mode {
        let warmup_time = value_t!(matches.value_of("warmup_time"), f32)
            .unwrap()
            .max(0.0); // has default value
        let record_time = value_t!(matches.value_of("reclen"), f32).unwrap().max(0.0); // has default value
        let output_filename = matches.value_of("output_file").unwrap(); // has default value

        println!("Warming up..");

        // warm up
        generator.generate(&mut vec![0.0; seconds_to_samples(warmup_time, sample_rate)]);

        println!("Recording..");

        // record
        let mut output = vec![0.0; seconds_to_samples(record_time, sample_rate)];

        generator.generate(&mut output);

        if matches.occurrences_of("crossfade") != 0 {
            let crossfade_duration = value_t!(matches.value_of("crossfade"), f32).unwrap();
            let crossfade_size = seconds_to_samples(
                crossfade_duration.max(1.0 / sample_rate as f32),
                sample_rate,
            );

            if crossfade_size >= output.len() {
                println!("Crossfade duration is too long {}", crossfade_duration);
                std::process::exit(4);
            }

            println!("Crossfading..");

            let len = output.len();
            let half_len = len / 2;

            let mut shifted = output.clone();

            shifted
                .iter_mut()
                .enumerate()
                .for_each(|(i, x)| *x = output[(half_len + i) % len]);

            output = Vec::with_capacity(shifted.len() - crossfade_size / 2);
            output.extend_from_slice(&shifted[..half_len]);
            output.extend_from_slice(&shifted[(half_len + crossfade_size / 2)..]);

            let fade_len = crossfade_size / 2;
            let start = half_len - fade_len;
            let end = half_len;
            for i in start..end {
                let fade = (i - start) as f32 / fade_len as f32;
                output[i] = shifted[i] * (1.0 - fade) + shifted[i + fade_len] * fade;
            }
        }

        let mut recorder = Recorder::new(output_filename.into(), sample_rate);

        println!("Started recording to \"{}\"", output_filename);

        // records into wav file asynchronously
        recorder.record(output.to_vec());
        recorder.stop_wait();
    } else {
        #[cfg(not(feature = "gui"))]
        {
            eprintln!("Headless builds do not supply GUI");
        }
        #[cfg(feature = "gui")]
        {
            let generator = Arc::new(RwLock::new(generator));

            let (audio, fft_receiver) = match audio::init(generator.clone(), sample_rate) {
                Ok(audio) => audio,
                Err(e) => {
                    eprintln!("Failed to initialize audio output: {}", e);
                    eprintln!("Continuing without audio playback.");
                    audio::init_no_output(generator.clone(), sample_rate)
                }
            };

            // this channel is bounded in practice by the channel between the following ExactStreamer of the FFTStreamer and it's channel's capacity (created in crate::audio::init)
            let (fft_sender, gui_fft_receiver) = crossbeam_channel::bounded(4);

            let mut fft = FFTStreamer::new(
                WATERFALL_WIDTH as usize * 2, /* only half of the spectrum can be used */
                ExactStreamer::new(GENERATOR_BUFFER_SIZE, fft_receiver),
                fft_sender,
            );

            // spawns thread for fft to create the waterfall lines
            std::thread::spawn(move || {
                fft.run();
            });

            if std::env::var_os("WAYLAND_DISPLAY").is_some()
                && std::env::var_os("WINIT_UNIX_BACKEND").is_none()
            {
                std::env::set_var("WINIT_UNIX_BACKEND", "wayland");
            }

            let allow_drag_drop = !matches.is_present("no-drag-drop");
            let app = gui::EngineSoundApp::new(
                generator.clone(),
                gui_fft_receiver,
                audio,
                sample_rate,
                allow_drag_drop,
            );

            let options = eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_title("Engine Sound Generator")
                    .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT]),
                ..Default::default()
            };

            if let Err(e) = eframe::run_native(
                "Engine Sound Generator",
                options,
                Box::new(|_cc| Ok(Box::new(app))),
            ) {
                eprintln!("Failed to start GUI: {e}");
            }
        }
    }
}
