use std::{
    collections::{HashSet, VecDeque},
    ffi::OsStr,
    fs::{self, File, FileTimes, OpenOptions},
    io::Write,
    num::NonZero,
    path::PathBuf,
    process::Command,
    sync::{Arc, Condvar, Mutex},
    thread::{available_parallelism, sleep},
    time::Duration,
};

#[cfg(target_os = "windows")]
use std::os::windows::{fs::FileTimesExt, process::CommandExt};

#[cfg(all(not(target_os = "hermit"), any(unix)))]
use nix::sys::signal::{SigHandler, Signal, signal};
#[cfg(all(not(target_os = "hermit"), any(unix)))]
use std::os::unix::process::CommandExt;

use clap::Parser;
use walkdir::WalkDir;

#[derive(Default)]
struct State {
    jobs: VecDeque<(PathBuf, PathBuf)>,
    cancel: bool,
    done: bool,
}

#[derive(Clone, Copy)]
enum ArgMode {
    Image,
    Video,
    Custom,
}

fn main() {
    let cli = Cli::parse();

    let core = cli.jobs.max(1).min(
        available_parallelism()
            .unwrap_or(unsafe { NonZero::new_unchecked(1) })
            .get(),
    );

    let img_exts = ["png", "jpg", "jpeg", "bmp", "tiff", "gif"];

    let vid_exts = [
        "mp4", "mkv", "mov", "avi", "webm", "flv", "wmv", "m4v", "mpg", "mpeg", "3gp", "ts",
        "m2ts", "mts", "ogv", "f4v", "vob", "asf", "rm", "rmvb",
    ];

    let exts = if cli.video {
        vid_exts.as_slice()
    } else {
        img_exts.as_slice()
    };

    // let mut counter = 0;
    let output = PathBuf::from(cli.log);

    // if output.exists() {
    //     loop {
    //         let file_name = output.file_stem().unwrap_or_default().to_string_lossy();
    //         let new_output = output.with_file_name(format!("{file_name}_{counter}.txt"));

    //         counter += 1;

    //         if !new_output.exists() {
    //             output = new_output;
    //             break;
    //         }
    //     }
    // }

    let pair = Arc::new((
        Mutex::new(State::default()),
        Condvar::new(),
        Mutex::new(
            OpenOptions::new()
                .write(true)
                .create(true)
                .open(&output)
                .unwrap(),
        ),
        Mutex::new((0u64, 0u64)),
    ));

    {
        let pair = pair.clone();

        ctrlc::set_handler(move || {
            eprintln!("CONTROL C");
            let (mutex, cond, _, _) = &*(pair);
            mutex.lock().unwrap().cancel = true;
            cond.notify_all();
        })
        .expect("Error setting Ctrl-C handler");
    }

    let mut threads = Vec::with_capacity(core);

    let arg_mode = if cli.args.is_some() {
        ArgMode::Custom
    } else if cli.video {
        ArgMode::Video
    } else {
        ArgMode::Image
    };

    let use_extension = if cli.video { "webm" } else { "webp" };

    for _ in 0..core {
        let pair = pair.clone();
        let dry = cli.dry;
        let video = cli.video;

        let thread = std::thread::spawn(move || {
            loop {
                let work;

                {
                    let (mutex, cond, _, _) = &*(pair);

                    let mut state = mutex.lock().unwrap();

                    loop {
                        if state.cancel || (state.done && state.jobs.len() == 0) {
                            println!("Thread quit!");
                            return;
                        }

                        if state.jobs.len() != 0 {
                            break;
                        }

                        state = cond.wait(state).unwrap();
                    }

                    work = state.jobs.pop_front();
                }

                if let Some(work) = work {
                    let arg_image = [
                        OsStr::new("-hide_banner"),
                        OsStr::new("-n"),
                        OsStr::new("-i"),
                        work.0.as_os_str(),
                        OsStr::new("-vcodec"),
                        OsStr::new("libwebp"),
                        OsStr::new("-qscale"),
                        OsStr::new("80"),
                        work.1.as_os_str(),
                    ];

                    let metadata = fs::metadata(work.0.as_os_str());

                    let input_size = {
                        if let Ok(m) = fs::metadata(work.0.as_os_str()) {
                            m.len()
                        } else {
                            0
                        }
                    };

                    let arg_video = [
                        OsStr::new("-hide_banner"),
                        OsStr::new("-n"),
                        OsStr::new("-i"),
                        work.0.as_os_str(),
                        OsStr::new("-vf"),
                        OsStr::new(
                            "scale='min(1280,iw)':'min(720,ih)':force_original_aspect_ratio=decrease",
                        ),
                        OsStr::new("-c:v"),
                        OsStr::new("libvpx-vp9"),
                        OsStr::new("-crf"),
                        OsStr::new("33"),
                        OsStr::new("-b:v"),
                        OsStr::new("0"),
                        OsStr::new("-speed"),
                        OsStr::new("4"),
                        OsStr::new("-row-mt"),
                        OsStr::new("1"),
                        OsStr::new("-threads"),
                        OsStr::new("4"),
                        OsStr::new("-c:a"),
                        OsStr::new("libopus"),
                        OsStr::new("-b:a"),
                        OsStr::new("96k"),
                        work.1.as_os_str(),
                    ];

                    let args = match arg_mode {
                        ArgMode::Image => arg_image.as_slice(),
                        ArgMode::Video => arg_video.as_slice(),
                        ArgMode::Custom => {
                            todo!()
                        }
                    };

                    if dry != 0 {
                        print!("ffmpeg");
                        for arg in args {
                            print!("{:?}", arg);
                        }
                        print!("\n");

                        sleep(Duration::from_millis(dry as _));
                    } else {
                        let mut command = Command::new("ffmpeg");
                        command.args(args);

                        #[cfg(target_os = "windows")]
                        command.creation_flags(0x00000200);

                        #[cfg(all(not(target_os = "hermit"), any(unix)))]
                        unsafe {
                            command.pre_exec(|| {
                                // Ignore SIGINT in the child
                                unsafe {
                                    signal(Signal::SIGINT, SigHandler::SigIgn).unwrap();
                                }
                                Ok(())
                            })
                        };

                        if video {
                            println!("Start {:?} {:?}", work.0, work.1);
                        }

                        let status = if video {
                            command.spawn().unwrap().wait_with_output().unwrap()
                        } else {
                            command.output().unwrap()
                        };

                        {
                            let mut file = pair.2.lock().unwrap();
                            file.write_all(&status.stdout).unwrap();
                            file.write_all(&status.stderr).unwrap();
                        }

                        if status.status.success() {
                            if video {
                                print!("End: ")
                            }

                            let output_size = {
                                if let Ok(m) = fs::metadata(work.1.as_os_str()) {
                                    m.len()
                                } else {
                                    0
                                }
                            };

                            println!("{:?} {:?}", work.0, work.1);
                            std::fs::remove_file(work.0).unwrap();

                            let mut t = pair.3.lock().unwrap();

                            if let Ok(metadata) = metadata {
                                if let Ok(file) =
                                    OpenOptions::new().write(true).create(true).open(&work.1)
                                {
                                    let mut times = FileTimes::new();

                                    #[cfg(target_os = "windows")]
                                    if let Ok(t) = metadata.created() {
                                        times = times.set_created(t);
                                    }
                                    if let Ok(t) = metadata.modified() {
                                        times = times.set_modified(t);
                                    }
                                    if let Ok(t) = metadata.accessed() {
                                        times = times.set_accessed(t);
                                    }

                                    if let Err(e) = file.set_times(times) {
                                        eprintln!("Failed to set metadata of file {:?} {e}", work.1)
                                    }
                                }
                            }

                            t.0 += input_size;
                            t.1 += output_size;
                        } else {
                            eprintln!("Failed: {:?} {:?}", work.0, work.1);
                            std::io::stderr().write_all(&status.stderr).unwrap();

                            if video {
                                std::fs::remove_file(work.1).unwrap();
                            }
                        }
                    }
                }
            }
        });

        threads.push(thread);
    }

    let mut outputs = HashSet::new();

    'outer: for entry in WalkDir::new(cli.folder)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();

        let Some(ext) = path.extension().and_then(OsStr::to_str) else {
            continue;
        };

        if exts.iter().any(|&e| e.eq_ignore_ascii_case(ext)) {
            if pair.0.lock().unwrap().cancel {
                break 'outer;
            }

            let (mutex, cond, _, _) = &*(pair);

            let mut counter = 0;
            let mut output = path.with_extension(use_extension);

            if output.exists() || outputs.contains(&output) {
                loop {
                    let file_name = output.file_stem().unwrap_or_default().to_string_lossy();
                    let new_output =
                        output.with_file_name(format!("{file_name}_{counter}.{use_extension}"));

                    counter += 1;

                    if !new_output.exists() && !outputs.contains(&new_output) {
                        output = new_output;
                        break;
                    }
                }
            }

            outputs.insert(output.clone());

            mutex
                .lock()
                .unwrap()
                .jobs
                .push_back((path.to_owned(), output));

            cond.notify_all();
        }
    }

    println!("Done search!");
    println!("Jobs {} left", pair.0.lock().unwrap().jobs.len());

    pair.0.lock().unwrap().done = true;
    pair.1.notify_all();

    for thread in threads {
        thread.join().unwrap();
    }

    println!("Jobs {} left", pair.0.lock().unwrap().jobs.len());

    pair.2.lock().unwrap().flush().unwrap();

    let data = pair.3.lock().unwrap().clone();

    fn format_bytes(bytes: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;

        match bytes {
            b if b >= GB => format!("{:.2} GiB", b as f64 / GB as f64),
            b if b >= MB => format!("{:.2} MiB", b as f64 / MB as f64),
            b if b >= KB => format!("{:.2} KiB", b as f64 / KB as f64),
            b => format!("{} B", b),
        }
    }

    println!(
        "Input {}\nOutput {}\nDiff {}",
        format_bytes(data.0),
        format_bytes(data.1),
        format_bytes(data.0 - data.1)
    );

    drop(pair);

    if output.metadata().unwrap().len() == 0 {
        std::fs::remove_file(output).unwrap();
    }
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Folder to work.
    folder: PathBuf,

    /// Core count
    #[arg(default_value_t = 8, short = 'j')]
    jobs: usize,

    #[arg(default_value_t = 0, short = 'd')]
    dry: usize,

    #[arg(default_value_t = String::from("ffpack.txt"), short = 'l')]
    log: String,

    #[arg(default_value_t = false, short = 'v')]
    video: bool,

    args: Option<String>,
}
