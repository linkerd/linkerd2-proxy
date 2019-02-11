#[macro_use]
extern crate log;
#[macro_use]
extern crate futures;
extern crate futures_watch;
extern crate ring;
extern crate tokio_timer;

use std::{
    cell::RefCell,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use futures::Stream;
use ring::digest::{self, Digest};

use tokio_timer::{clock, Interval};

mod either_stream;

/// Stream changes to the files at a group of paths.
pub fn stream_changes<I, P>(paths: I, interval: Duration) -> impl Stream<Item = (), Error = ()>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    // If we're on Linux, first atttempt to start an Inotify watch on the
    // paths. If this fails, fall back to polling the filesystem.
    #[cfg(target_os = "linux")]
    {
        stream_changes_inotify(paths, interval)
    }

    // If we're not on Linux, we can't use inotify, so simply poll the fs.
    // TODO: Use other FS events APIs (such as `kqueue`) as well, when
    //       they're available.
    #[cfg(not(target_os = "linux"))]
    {
        stream_changes_polling(paths, interval)
    }
}

/// Stream changes by polling the filesystem.
///
/// This will calculate the SHA-384 hash of each of files at the paths
/// described by this `CommonSettings` every `interval`, and attempt to
/// load a new `CommonConfig` from the files again if any of the hashes
/// has changed.
///
/// This is used on operating systems other than Linux, or on Linux if
/// our attempt to use `inotify` failed.
pub fn stream_changes_polling<I, P>(
    paths: I,
    interval: Duration,
) -> impl Stream<Item = (), Error = ()>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let files = paths.into_iter().map(PathAndHash::new).collect::<Vec<_>>();

    Interval::new(clock::now(), interval)
        .map_err(|e| error!("timer error: {:?}", e))
        .filter(move |_| {
            let mut any_changes = false;
            for file in &files {
                let has_changed = file.update_and_check().unwrap_or_else(|e| {
                    warn!("error hashing {:?}: {}", &file.path, e);
                    false
                });
                if has_changed {
                    any_changes = true;
                }
            }
            any_changes
        })
        .map(|_| ())
}

#[cfg(target_os = "linux")]
pub fn stream_changes_inotify<I, P>(
    paths: I,
    interval: Duration,
) -> impl Stream<Item = (), Error = ()>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    use either_stream::EitherStream;

    let paths: Vec<PathBuf> = paths
        .into_iter()
        .map(|p| p.as_ref().to_path_buf())
        .collect();
    let polls = Box::new(stream_changes_polling(paths.clone(), interval));
    match inotify::WatchStream::new(paths) {
        Ok(watch) => {
            let stream = inotify::FallbackStream { watch, polls };
            EitherStream::A(stream)
        }
        Err(e) => {
            // If initializing the `Inotify` instance failed, it probably won't
            // succeed in the future (it's likely that inotify unsupported on
            // this OS).
            warn!("inotify init error: {}, falling back to polling", e);
            EitherStream::B(polls)
        }
    }
}

#[derive(Clone, Debug)]
struct PathAndHash {
    /// The path to the file.
    path: PathBuf,

    /// The last SHA-384 digest of the file, if we have previously hashed it.
    curr_hash: RefCell<Option<Digest>>,
}

impl PathAndHash {
    fn new<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            curr_hash: RefCell::new(None),
        }
    }

    /// Attempts to update the hash for this file and checks if it has changed.
    ///
    /// # Returns
    /// - `Ok(true)` if the file was read successfully and the hash has changed.
    /// - `Ok(false)` if we were able to read the file but the has has not
    ///    changed.
    /// - `Err(io::Error)` if an error occurred reading the file.
    fn update_and_check(&self) -> io::Result<bool> {
        match fs::read(&self.path) {
            Ok(contents) => {
                // If we were able to read the file, compare the hash of its
                // current contents with the previous hash to determine if it
                // has changed.
                let curr_hash = Some(digest::digest(&digest::SHA256, &contents[..]));
                let changed = {
                    // We can't compare `ring::Digest`s directly, so we have to
                    // borrow the hashes as byte slices to compare them.
                    let prev_hash = self.curr_hash.borrow();
                    let prev_hash_bytes = prev_hash.as_ref().map(Digest::as_ref);
                    let curr_hash_bytes = curr_hash.as_ref().map(Digest::as_ref);
                    prev_hash_bytes != curr_hash_bytes
                };
                if changed {
                    trace!("{:?} changed", &self.path);
                    self.curr_hash.replace(curr_hash);
                }
                Ok(changed)
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
                if self.curr_hash.borrow().is_some() {
                    // If we have a previous hash, then the file was deleted,
                    // so it has changed.
                    trace!("{:?} deleted", &self.path);
                    self.curr_hash.replace(None);
                    Ok(true)
                } else {
                    // Otherwise, it didn't exist previously, so there hasn't
                    // been a change.
                    Ok(false)
                }
            }
            // Propagate any other errors.
            Err(e) => Err(e),
        }
    }
}

#[cfg(target_os = "linux")]
pub mod inotify {
    extern crate inotify;

    use self::inotify::{Event, EventMask, EventStream, Inotify, WatchMask};
    use futures::{Async, Poll, Stream};
    use std::{io, path::PathBuf};

    pub struct WatchStream {
        inotify: Inotify,
        stream: EventStream<[u8; 32]>,
        paths: Vec<PathBuf>,
    }

    pub struct FallbackStream {
        pub watch: WatchStream,
        pub polls: Box<Stream<Item = (), Error = ()> + Send>,
    }

    impl WatchStream {
        pub fn new(paths: Vec<PathBuf>) -> Result<Self, io::Error> {
            let mut inotify = Inotify::init()?;
            let stream = inotify.event_stream([0; 32]);

            let mut watch_stream = WatchStream {
                inotify,
                stream,
                paths,
            };

            watch_stream.add_paths()?;

            Ok(watch_stream)
        }

        fn add_paths(&mut self) -> Result<(), io::Error> {
            let mask = WatchMask::CREATE
                | WatchMask::MODIFY
                | WatchMask::DELETE
                | WatchMask::DELETE_SELF
                | WatchMask::MOVE
                | WatchMask::MOVE_SELF;
            for path in &self.paths {
                let watch_path = path.canonicalize().unwrap_or_else(|e| {
                    trace!("canonicalize({:?}): {:?}", &path, e);
                    path.parent().unwrap_or_else(|| path.as_ref()).to_path_buf()
                });
                self.inotify.add_watch(&watch_path, mask)?;
                trace!("watch {:?} (for {:?})", watch_path, path);
            }
            Ok(())
        }
    }

    impl Stream for WatchStream {
        type Item = ();
        type Error = io::Error;
        fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
            loop {
                match try_ready!(self.stream.poll()) {
                    Some(Event { mask, name, .. }) => {
                        if mask.contains(EventMask::IGNORED) {
                            // This event fires if we removed a watch. Poll the
                            // stream again.
                            continue;
                        }
                        trace!("event={:?}; path={:?}", mask, name);
                        if mask.contains(
                            EventMask::DELETE & EventMask::DELETE_SELF & EventMask::CREATE,
                        ) {
                            self.add_paths()?;
                        }
                        return Ok(Async::Ready(Some(())));
                    }
                    None => {
                        debug!("watch stream ending");
                        return Ok(Async::Ready(None));
                    }
                }
            }
        }
    }

    impl Stream for FallbackStream {
        type Item = ();
        type Error = ();
        fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
            self.watch.poll().or_else(|e| {
                warn!("watch error: {:?}, polling the fs until next change", e);
                self.polls.poll()
            })
        }
    }

}

#[cfg(test)]
mod tests {
    extern crate env_logger;
    extern crate linkerd2_task as task;
    extern crate tempfile;
    extern crate tokio;

    use self::task::test_util::BlockOnFor;
    use self::tokio::runtime::current_thread::Runtime;
    use super::*;
    use tokio_timer::{clock, Interval};

    #[cfg(not(target_os = "windows"))]
    use std::os::unix::fs::symlink;
    use std::{
        fs::{self, File},
        io::Write,
        path::Path,
        time::Duration,
    };

    use futures::{future, Future, Sink, Stream};
    use futures_watch::{Watch, WatchError};

    struct Fixture {
        paths: Vec<PathBuf>,
        dir: tempfile::TempDir,
        rt: Runtime,
    }

    impl Fixture {
        fn new() -> Fixture {
            let _ = self::env_logger::try_init();
            let dir = tempfile::Builder::new().prefix("test").tempdir().unwrap();
            let paths = vec![
                dir.path().join("a"),
                dir.path().join("b"),
                dir.path().join("c"),
            ];
            let rt = Runtime::new().unwrap();
            Fixture { paths, dir, rt }
        }

        fn test_polling(self, test: fn(Self, Watch<()>, Box<Future<Item = (), Error = ()>>)) {
            let stream = stream_changes_polling(self.paths.clone(), Duration::from_secs(1));
            let (watch, bg) = watch_stream(stream);
            test(self, watch, bg)
        }

        #[cfg(target_os = "linux")]
        fn test_inotify(self, test: fn(Self, Watch<()>, Box<Future<Item = (), Error = ()>>)) {
            let paths = self.paths.clone();
            let stream = inotify::WatchStream::new(paths)
                .unwrap()
                .map_err(|e| panic!("{}", e));
            let (watch, bg) = watch_stream(stream);
            test(self, watch, bg)
        }
    }

    fn create_file<P: AsRef<Path>>(path: P) -> io::Result<File> {
        let f = File::create(path)?;
        println!("created {:?}", f);
        Ok(f)
    }

    fn create_and_write<P: AsRef<Path>>(path: P, s: &[u8]) -> io::Result<File> {
        let mut f = File::create(path)?;
        f.write_all(s)?;
        println!("created and wrote to {:?}", f);
        Ok(f)
    }

    fn watch_stream(
        stream: impl Stream<Item = (), Error = ()> + 'static,
    ) -> (Watch<()>, Box<Future<Item = (), Error = ()>>) {
        let (watch, store) = Watch::new(());
        // Use a watch so we can start running the stream immediately but also
        // wait on stream updates.
        let f = stream
            .forward(store.sink_map_err(|_| ()))
            .map(|_| ())
            .map_err(|_| ());

        (watch, Box::new(f))
    }

    fn next_change(
        rt: &mut Runtime,
        watch: Watch<()>,
    ) -> Result<(Option<()>, Watch<()>), WatchError> {
        let next = watch.into_future().map_err(|(e, _)| e);
        // Rust will print a warning if a test runs longer than 60 seconds,
        // so we'll use that as the timeout after which we'll kill the test
        // if we don't see a change.
        rt.block_on_for(Duration::from_secs(60), next)
    }

    fn test_detects_create(
        fixture: Fixture,
        watch: Watch<()>,
        bg: Box<Future<Item = (), Error = ()>>,
    ) {
        let Fixture {
            paths,
            dir: _dir,
            mut rt,
        } = fixture;

        rt.spawn(bg);

        paths.iter().fold(watch, |watch, path| {
            create_file(path).unwrap();

            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });
    }

    fn test_detects_delete_and_recreate(
        fixture: Fixture,
        watch: Watch<()>,
        bg: Box<Future<Item = (), Error = ()>>,
    ) {
        let Fixture {
            paths,
            dir: _dir,
            mut rt,
        } = fixture;
        rt.spawn(bg);

        let watch = paths.iter().fold(watch, |watch, ref path| {
            create_and_write(path, b"A").unwrap();

            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });

        let watch = paths.iter().fold(watch, |watch, ref path| {
            fs::remove_file(path).unwrap();
            println!("deleted {:?}", path);

            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });

        paths.iter().fold(watch, |watch, ref path| {
            create_and_write(path, b"B").unwrap();

            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });
    }

    fn test_nonexistent_files_dont_file_delete_events(
        fixture: Fixture,
        watch: Watch<()>,
        bg: Box<Future<Item = (), Error = ()>>,
    ) {
        // This test confirms that when a file has been deleted,
        // it's nonexistence doesn't continuously generate new
        // "file deleted" events.
        let Fixture {
            paths,
            dir: _dir,
            mut rt,
        } = fixture;

        rt.spawn(bg);

        let watch = paths.iter().fold(watch, |watch, ref path| {
            create_and_write(path, b"A").unwrap();

            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });

        let watch = paths.iter().fold(watch, |watch, ref path| {
            fs::remove_file(path).unwrap();
            println!("deleted {:?}", path);

            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });

        let watch2 = watch.clone();
        // Check if the stream has become ready every one second.
        let stream = Interval::new(clock::now(), Duration::from_secs(1))
            .map(|_| {
                let mut watch = watch2.clone();
                // The stream should not be ready, since the file
                // system hasn't changed yet.
                assert!(!watch.poll().unwrap().is_ready());
                ()
            })
            .take(5)
            .fold((), |_, ()| future::ok(()));

        rt.block_on(stream).unwrap();

        // Creating the files again should generate a new event
        paths.iter().fold(watch, |watch, ref path| {
            create_and_write(path, b"B").unwrap();

            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });
    }

    #[cfg(not(target_os = "windows"))]
    fn test_detects_create_symlink(
        fixture: Fixture,
        watch: Watch<()>,
        bg: Box<Future<Item = (), Error = ()>>,
    ) {
        let Fixture { paths, dir, mut rt } = fixture;

        let data_path = dir.path().join("data");
        fs::create_dir(&data_path).unwrap();

        let data_paths = paths
            .iter()
            .map(|p| {
                let path = data_path.clone().join(p.file_name().unwrap());
                create_file(&path).unwrap();
                path
            })
            .collect::<Vec<_>>();

        rt.spawn(bg);

        data_paths
            .iter()
            .zip(paths.iter())
            .fold(watch, |watch, (path, target_path)| {
                symlink(path, target_path).unwrap();

                let (item, watch) = next_change(&mut rt, watch).unwrap();
                assert!(item.is_some());
                watch
            });
    }

    // Test for when the watched files are symlinks to a file inside of a
    // directory which is also a symlink (as is the case with Kubernetes
    // ConfigMap/Secret volume mounts).
    #[cfg(not(target_os = "windows"))]
    fn test_detects_create_double_symlink(
        fixture: Fixture,
        watch: Watch<()>,
        bg: Box<Future<Item = (), Error = ()>>,
    ) {
        let Fixture { paths, dir, mut rt } = fixture;

        let real_data_path = dir.path().join("real_data");
        let data_path = dir.path().join("data");
        fs::create_dir(&real_data_path).unwrap();
        symlink(&real_data_path, &data_path).unwrap();

        for path in &paths {
            let path = real_data_path.clone().join(path.file_name().unwrap());
            create_file(&path).unwrap();
        }

        // -- Below this point, the watch is running. -----------------------
        rt.spawn(bg);

        paths.iter().fold(watch, |watch, path| {
            let file_name = path.file_name().unwrap();
            symlink(data_path.clone().join(file_name), path).unwrap();

            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });
    }

    #[cfg(not(target_os = "windows"))]
    fn test_detects_modification_symlink(
        fixture: Fixture,
        watch: Watch<()>,
        bg: Box<Future<Item = (), Error = ()>>,
    ) {
        let Fixture { paths, dir, mut rt } = fixture;

        let data_path = dir.path().join("data");
        fs::create_dir(&data_path).unwrap();

        let data_paths = paths
            .iter()
            .map(|p| {
                let path = data_path.clone().join(p.file_name().unwrap());
                path
            })
            .collect::<Vec<_>>();

        let mut data_files = data_paths
            .iter()
            .map(|path| create_and_write(path, b"a").unwrap())
            .collect::<Vec<_>>();

        for (path, target_path) in data_paths.iter().zip(paths.iter()) {
            // Don't assert that events are seen here, as we haven't started
            // running the watch yet.
            symlink(path, target_path).unwrap();
        }

        // -- Below this point, the watch is running. -----------------------
        rt.spawn(bg);

        data_files.iter_mut().fold(watch, |watch, file| {
            file.write_all(b"b").unwrap();

            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });
    }

    fn test_detects_modification(
        fixture: Fixture,
        watch: Watch<()>,
        bg: Box<Future<Item = (), Error = ()>>,
    ) {
        let Fixture {
            paths,
            dir: _dir,
            mut rt,
        } = fixture;

        let mut files = paths
            .iter()
            .map(|path| create_and_write(path, b"a").unwrap())
            .collect::<Vec<_>>();

        rt.spawn(bg);

        files.iter_mut().fold(watch, |watch, file| {
            file.write_all(b"b").unwrap();
            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });
    }

    #[cfg(not(target_os = "windows"))]
    fn test_detects_modification_double_symlink(
        fixture: Fixture,
        watch: Watch<()>,
        bg: Box<Future<Item = (), Error = ()>>,
    ) {
        let Fixture { paths, dir, mut rt } = fixture;

        let real_data_path = dir.path().join("real_data");
        let data_path = dir.path().join("data");
        fs::create_dir(&real_data_path).unwrap();
        symlink(&real_data_path, &data_path).unwrap();

        let mut files = paths
            .iter()
            .map(|p| {
                let path = real_data_path.clone().join(p.file_name().unwrap());
                create_and_write(path, b"a").unwrap()
            })
            .collect::<Vec<_>>();

        for path in &paths {
            let file_path = data_path.clone().join(path.file_name().unwrap());
            // Don't assert that events are seen here, as we haven't started
            // running the watch yet.
            symlink(file_path, path).unwrap();
        }

        // -- Below this point, the watch is running. -----------------------
        rt.spawn(bg);

        files.iter_mut().fold(watch, |watch, file| {
            file.write_all(b"b").unwrap();
            let (item, watch) = next_change(&mut rt, watch).unwrap();
            assert!(item.is_some());
            watch
        });
    }

    #[cfg(not(target_os = "windows"))]
    fn test_detects_double_symlink_retargeting(
        fixture: Fixture,
        watch: Watch<()>,
        bg: Box<Future<Item = (), Error = ()>>,
    ) {
        let Fixture { paths, dir, mut rt } = fixture;

        let real_data_path = dir.path().join("real_data");
        let real_data_path_2 = dir.path().join("real_data_2");
        let data_path = dir.path().join("data");
        fs::create_dir(&real_data_path).unwrap();
        fs::create_dir(&real_data_path_2).unwrap();
        symlink(&real_data_path, &data_path).unwrap();

        // Create the first set of files.
        // We won't assert that any changes are detected until we actually
        // start the watch.
        for path in &paths {
            let path = real_data_path.clone().join(path.file_name().unwrap());
            create_and_write(path, b"a").unwrap();
        }

        // Symlink those files into `data_path`
        for path in &paths {
            let data_file_path = data_path.clone().join(path.file_name().unwrap());
            symlink(data_file_path, path).unwrap();
        }

        // Create the second set of files.
        for path in &paths {
            let path = real_data_path_2.clone().join(path.file_name().unwrap());
            create_and_write(path, b"b").unwrap();
        }

        // -- Below this point, the watch is running. -----------------------
        rt.spawn(bg);

        let (item, watch) = next_change(&mut rt, watch).unwrap();
        assert!(item.is_some());

        fs::remove_dir_all(&data_path).unwrap();
        symlink(&real_data_path_2, &data_path).unwrap();

        let (item, _) = next_change(&mut rt, watch).unwrap();
        assert!(item.is_some());
    }

    #[test]
    fn polling_detects_create() {
        Fixture::new().test_polling(test_detects_create)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_create() {
        Fixture::new().test_inotify(test_detects_create)
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn polling_detects_create_symlink() {
        Fixture::new().test_polling(test_detects_create_symlink)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_create_symlink() {
        Fixture::new().test_inotify(test_detects_create_symlink)
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn polling_detects_create_double_symlink() {
        Fixture::new().test_polling(test_detects_create_double_symlink)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_create_double_symlink() {
        Fixture::new().test_inotify(test_detects_create_double_symlink)
    }

    #[test]
    fn polling_detects_modification() {
        Fixture::new().test_polling(test_detects_modification)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_modification() {
        Fixture::new().test_inotify(test_detects_modification)
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn polling_detects_modification_symlink() {
        Fixture::new().test_polling(test_detects_modification_symlink)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_modification_symlink() {
        Fixture::new().test_inotify(test_detects_modification_symlink)
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn polling_detects_modification_double_symlink() {
        Fixture::new().test_polling(test_detects_modification_double_symlink)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_modification_double_symlink() {
        Fixture::new().test_inotify(test_detects_modification_double_symlink)
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn polling_detects_double_symlink_retargeting() {
        Fixture::new().test_polling(test_detects_double_symlink_retargeting)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_double_symlink_retargeting() {
        Fixture::new().test_inotify(test_detects_double_symlink_retargeting)
    }

    #[test]
    fn polling_detects_delete_and_recreate() {
        Fixture::new().test_polling(test_detects_delete_and_recreate)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_delete_and_recreate() {
        Fixture::new().test_inotify(test_detects_delete_and_recreate)
    }

    #[test]
    fn polling_nonexistent_files_dont_file_delete_events() {
        Fixture::new().test_polling(test_nonexistent_files_dont_file_delete_events)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_nonexistent_files_dont_file_delete_events() {
        Fixture::new().test_inotify(test_detects_delete_and_recreate)
    }
}
