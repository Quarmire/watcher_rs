use notify::{Watcher, RecommendedWatcher, RecursiveMode, Result, Config, Event, EventKind};
use std::fs::create_dir_all;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, mpsc::channel};
use std::process::Command;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::{env, fs, io};
use std::time::{Duration, SystemTime};
use nix;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() != 4 {
        println!("Usage {} <interval (sec)> <size_limit (MB)> <directory>", args[0]);
    }

    let interval: u64 = args[1].parse().unwrap_or(3);
    let size_limit: u64 = args[2].parse().unwrap_or(2048);
    let path_to_watch: String = args[3].clone();
    let path_to_watch = path_to_watch.as_str();

    // Ensure the folder exists
    create_dir_all(&path_to_watch)?;

    let (tx, rx) = channel();
    let mut watcher: RecommendedWatcher = Watcher::new(tx, Config::default()).unwrap();

    watcher.watch(Path::new(path_to_watch), RecursiveMode::Recursive)?;

    let active_threads: Arc<Mutex<Vec<(JoinHandle<()>, PathBuf, Sender<()>)>>> = Arc::new(Mutex::new(Vec::new()));
    let size_limit = size_limit * 1024 * 1024; // Size limit in bytes (100 MB here)

    // Existing files are put onto the NDN network
    {
        let active_threads = Arc::clone(&active_threads);
        let mut files: Vec<PathBuf> = vec![];
        collect_files_recursively(&Path::new(&path_to_watch), &mut files);
        for path in files {
            let path_clone = path.clone();
            let (stop_tx, stop_rx) = channel();

            let handle = thread::spawn(move || {
                execute_command_on_file(&path_clone, stop_rx);
            });
            println!("Adding existing file: {:?}", path.clone());

            active_threads.lock().unwrap().push((handle, path, stop_tx));
        }
    }

    println!("Watching directory: {:?}", path_to_watch);

    // Periodic size checker thread
    {
        let path_to_watch = path_to_watch.to_string();
        let active_threads = Arc::clone(&active_threads);

        thread::spawn(move || loop {
            if let Ok(size) = get_dir_size(Path::new(&path_to_watch)) {
                // println!("Directory size: {} bytes", size);
                if size > size_limit {
                    println!("Size limit exceeded. Stopping oldest threads and deleting files...");
                    let mut threads = active_threads.lock().unwrap();

                    if !threads.is_empty() {
                        // Remove oldest threads to keep size below the limit
                        while get_dir_size(Path::new(&path_to_watch)).unwrap_or(size) > size_limit && !threads.is_empty() {
                            let (handle, file_path, stop_tx) = threads.remove(0); // Remove the tuple of thread and file path
                            stop_tx.send(()).unwrap();
                            handle.join().expect("Failed to join thread");
                            if let Err(e) = fs::remove_file(&file_path) {
                                eprintln!("Failed to delete file {:?}: {:?}", file_path, e);
                            } else {
                                println!("Deleted file: {:?}", file_path);
                            }
                        }
                    } else {
                        // No active threads, delete files by the oldest date
                        let mut files: Vec<PathBuf> = vec![];
                        collect_files_recursively(&Path::new(&path_to_watch), &mut files);

                        // Sort files by modification date (oldest first)
                        files.sort_by_key(|path| {
                            fs::metadata(path)
                                .and_then(|metadata| metadata.created())
                                .unwrap_or(SystemTime::now())
                        });

                        println!("{:?}", files);

                        // Delete oldest files until size limit is respected
                        while get_dir_size(Path::new(&path_to_watch)).unwrap_or(size) > size_limit && !files.is_empty() {
                            if let Some(file_path) = files.pop() {
                                if let Err(e) = fs::remove_file(&file_path) {
                                    eprintln!("Failed to delete file {:?}: {:?}", file_path, e);
                                } else {
                                    println!("Deleted file: {:?}", file_path);
                                }
                            }
                        }
                    }
                }
            }
            thread::sleep(Duration::from_secs(interval)); // Check size every 3 seconds
        });
    }

    for res in rx {
        match res {
            Ok(event) => handle_event(event, Arc::clone(&active_threads)),
            Err(e) => eprintln!("watch error: {:?}", e),
        }
    }

    Ok(())
}

fn handle_event(event: Event, active_threads: Arc<Mutex<Vec<(JoinHandle<()>, PathBuf, Sender<()>)>>>) {
    if let EventKind::Create(_) = event.kind {
        for path in event.paths {
            if path.is_file() {
                println!("New file detected: {:?}", path);
                let path_copy = path.clone();
                let active_threads = Arc::clone(&active_threads);
                let (stop_tx, stop_rx) = channel();

                let handle = thread::spawn(move || {
                    execute_command_on_file(&path_copy, stop_rx);
                });

                active_threads.lock().unwrap().push((handle, path, stop_tx));
            }
        }
    }
}

fn execute_command_on_file(path: &Path, stop_rx: Receiver<()>) {
    let command = "ndnputchunks"; // Replace with the command you wish to run
    let file_name = path.file_name().unwrap().to_string_lossy().to_string();
    let child = Command::new(command)
        .arg(format!("/ndn/fossn/files/{}", file_name))
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let child_pid: i32 = child.id().try_into().unwrap();

    let mut f = fs::File::open(path).unwrap();
    io::copy(&mut f, &mut child.stdin.unwrap()).unwrap();

    match stop_rx.recv() {
        Ok(_) => { nix::sys::signal::kill(nix::unistd::Pid::from_raw(child_pid), nix::sys::signal::Signal::SIGKILL).unwrap(); },
        Err(e) => { println!("stop_rx error: {}", e); }
    };
}

fn get_dir_size(path: &Path) -> std::io::Result<u64> {
    let mut size = 0;

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            size += get_dir_size(&entry.path())?;
        } else {
            size += metadata.len();
        }
    }

    Ok(size)
}

fn collect_files_recursively(dir: &Path, files: &mut Vec<PathBuf>) {
    if dir.is_dir() {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                collect_files_recursively(&path, files);
            } else {
                files.push(path);
            }
        }
    }
}