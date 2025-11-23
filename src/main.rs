use std::{
    collections::{HashSet, VecDeque},
    ffi::OsStr,
    fs::File,
    io::Write,
    num::NonZero,
    path::PathBuf,
    process::Command,
    sync::{Arc, Condvar, Mutex},
    thread::{available_parallelism, sleep},
    time::Duration,
};

use clap::Parser;
use walkdir::WalkDir;

#[derive(Default)]
struct State {
    jobs: VecDeque<(PathBuf, PathBuf)>,
    cancel: bool,
    done: bool,
}

fn main() {
    let cli = Cli::parse();

    let core = cli.jobs.max(1).min(
        available_parallelism()
            .unwrap_or(unsafe { NonZero::new_unchecked(1) })
            .get(),
    );

    let exts = ["png", "jpg", "jpeg", "bmp", "tiff", "gif"];

    let mut counter = 0;
    let mut output = PathBuf::from(cli.log);

    if output.exists() {
        loop {
            let file_name = output.file_name().unwrap_or_default().to_string_lossy();
            let new_output = output.with_file_name(format!("{file_name}_{counter}"));

            counter += 1;

            if !new_output.exists() {
                output = new_output;
                break;
            }
        }
    }

    let pair = Arc::new((
        Mutex::new(State::default()),
        Condvar::new(),
        Mutex::new(File::create(output).unwrap()),
    ));

    {
        let pair = pair.clone();

        ctrlc::set_handler(move || {
            let (mutex, cond, _) = &*(pair);
            mutex.lock().unwrap().cancel = true;
            cond.notify_all();
        })
        .expect("Error setting Ctrl-C handler");
    }

    let mut threads = Vec::with_capacity(core);

    for _ in 0..core {
        let pair = pair.clone();
        let dry = cli.dry;

        let thread = std::thread::spawn(move || {
            loop {
                let work;

                {
                    let (mutex, cond, _) = &*(pair);

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
                    if dry != 0 {
                        println!(
                            "ffmpeg -i {:?} -vcodec libwebp -qscale 80 {:?}",
                            work.0, work.1
                        );

                        sleep(Duration::from_millis(dry as _));
                    } else {
                        let status = Command::new("ffmpeg")
                            .args([
                                OsStr::new("-hide_banner"),
                                OsStr::new("-n"),
                                OsStr::new("-i"),
                                work.0.as_os_str(),
                                OsStr::new("-vcodec"),
                                OsStr::new("libwebp"),
                                OsStr::new("-qscale"),
                                OsStr::new("80"),
                                work.1.as_os_str(),
                            ])
                            .output()
                            .unwrap();

                        {
                            let mut file = pair.2.lock().unwrap();
                            file.write_all(&status.stdout).unwrap();
                            file.write_all(&status.stderr).unwrap();
                        }

                        if status.status.success() {
                            std::fs::remove_file(work.0).unwrap();
                        } else {
                            eprintln!(
                                "Failed: ffmpeg -i {:?} -vcodec libwebp -qscale 80 {:?}",
                                work.0, work.1
                            );
                            std::io::stderr().write_all(&status.stderr).unwrap();
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

            let (mutex, cond, _) = &*(pair);

            let mut counter = 0;
            let mut output = path.with_extension("webp");

            if output.exists() || outputs.contains(&output) {
                loop {
                    let file_name = output.file_name().unwrap_or_default().to_string_lossy();
                    let new_output = output.with_file_name(format!("{file_name}_{counter}"));

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
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Folder to work.
    folder: PathBuf,

    /// Core count
    #[arg(default_value_t = 4, short = 'j')]
    jobs: usize,

    #[arg(default_value_t = 1000, short = 'd')]
    dry: usize,

    #[arg(default_value_t = String::from("ffpack.txt"), short = 'l')]
    log: String,
}
