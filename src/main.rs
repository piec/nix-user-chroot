use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd;
use nix::unistd::{fork, ForkResult};
use std::env;
use std::fs;
use std::io;
use std::io::prelude::*;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::string::String;
use tempfile::TempDir;

const NONE: Option<&'static [u8]> = None;

fn bind_mount(source: &Path, dest: &Path) {
    if let Err(e) = mount(
        Some(source),
        dest,
        Some("none"),
        MsFlags::MS_BIND | MsFlags::MS_REC,
        NONE,
    ) {
        eprintln!(
            "failed to bind mount {} to {}: {}",
            source.display(),
            dest.display(),
            e
        );
    }
}

pub struct RunChroot<'a> {
    rootdir: &'a Path,
}

impl<'a> RunChroot<'a> {
    fn new(rootdir: &'a Path) -> Self {
        Self { rootdir }
    }

    fn bind_mount_directory(&self, entry: &fs::DirEntry) {
        let mountpoint = self.rootdir.join(entry.file_name());
        if let Err(e) = fs::create_dir(&mountpoint) {
            if e.kind() != io::ErrorKind::AlreadyExists {
                let e2: io::Result<()> = Err(e);
                e2.unwrap_or_else(|_| panic!("failed to create {}", &mountpoint.display()));
            }
        }

        bind_mount(&entry.path(), &mountpoint)
    }

    fn bind_mount_file(&self, entry: &fs::DirEntry) {
        let mountpoint = self.rootdir.join(entry.file_name());
        fs::File::create(&mountpoint)
            .unwrap_or_else(|_| panic!("failed to create {}", &mountpoint.display()));

        bind_mount(&entry.path(), &mountpoint)
    }

    fn mirror_symlink(&self, entry: &fs::DirEntry) {
        let path = entry.path();
        let target = fs::read_link(&path)
            .unwrap_or_else(|_| panic!("failed to resolve symlink {}", &path.display()));
        let link_path = self.rootdir.join(entry.file_name());
        symlink(&target, &link_path).unwrap_or_else(|_| {
            panic!(
                "failed to create symlink {} -> {}",
                &link_path.display(),
                &target.display()
            )
        });
    }

    fn bind_mount_direntry(&self, entry: io::Result<fs::DirEntry>) {
        let entry = entry.expect("error while listing from /nix directory");
        // do not bind mount an existing nix installation
        if entry.file_name() == PathBuf::from("nix") {
            return;
        }
        let path = entry.path();
        let stat = entry
            .metadata()
            .unwrap_or_else(|_| panic!("cannot get stat of {}", path.display()));
        if stat.is_dir() {
            self.bind_mount_directory(&entry);
        } else if stat.is_file() {
            self.bind_mount_file(&entry);
        } else if stat.file_type().is_symlink() {
            self.mirror_symlink(&entry);
        }
    }

    fn run_chroot(&self, nixdir: &Path, cmd: &str, args: &[String]) {
        let cwd = env::current_dir().expect("cannot get current working directory");

        let uid = unistd::getuid();
        let gid = unistd::getgid();

        unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUSER).expect("unshare failed");

        // bind mount all / stuff into rootdir
        let nix_root = PathBuf::from("/");
        let dir = fs::read_dir(&nix_root).expect("failed to list /nix directory");
        for entry in dir {
            self.bind_mount_direntry(entry);
        }

        // mount the store
        let nix_mount = self.rootdir.join("nix");
        fs::create_dir(&nix_mount)
            .unwrap_or_else(|_| panic!("failed to create {}", &nix_mount.display()));
        mount(
            Some(nixdir),
            &nix_mount,
            Some("none"),
            MsFlags::MS_BIND | MsFlags::MS_REC,
            NONE,
        )
        .unwrap_or_else(|_| panic!("failed to bind mount {} to /nix", nixdir.display()));

        // chroot
        unistd::chroot(self.rootdir)
            .unwrap_or_else(|_| panic!("chroot({})", self.rootdir.display(),));

        env::set_current_dir("/").expect("cannot change directory to /");

        // fixes issue #1 where writing to /proc/self/gid_map fails
        // see user_namespaces(7) for more documentation
        if let Ok(mut file) = fs::File::create("/proc/self/setgroups") {
            let _ = file.write_all(b"deny");
        }

        let mut uid_map =
            fs::File::create("/proc/self/uid_map").expect("failed to open /proc/self/uid_map");
        uid_map
            .write_all(format!("{} {} 1", uid, uid).as_bytes())
            .expect("failed to write new uid mapping to /proc/self/uid_map");

        let mut gid_map =
            fs::File::create("/proc/self/gid_map").expect("failed to open /proc/self/gid_map");
        gid_map
            .write_all(format!("{} {} 1", gid, gid).as_bytes())
            .expect("failed to write new gid mapping to /proc/self/gid_map");

        // restore cwd
        env::set_current_dir(&cwd)
            .unwrap_or_else(|_| panic!("cannot restore working directory {}", cwd.display()));

        let err = process::Command::new(cmd)
            .args(args)
            .env("NIX_CONF_DIR", "/nix/etc/nix")
            .exec();

        eprintln!("failed to execute {}: {}", &cmd, err);
        process::exit(1);
    }
}

fn wait_for_child(child_pid: unistd::Pid, tempdir: TempDir, rootdir: &Path) {
    loop {
        match waitpid(child_pid, Some(WaitPidFlag::WUNTRACED)) {
            Ok(WaitStatus::Signaled(child, Signal::SIGSTOP, _)) => {
                let _ = kill(unistd::getpid(), Signal::SIGSTOP);
                let _ = kill(child, Signal::SIGCONT);
            }
            Ok(WaitStatus::Signaled(_, signal, _)) => {
                kill(unistd::getpid(), signal)
                    .unwrap_or_else(|_| panic!("failed to send {} signal to our self", signal));
            }
            Ok(WaitStatus::Exited(_, status)) => {
                tempdir.close().unwrap_or_else(|_| {
                    panic!(
                        "failed to remove temporary directory: {}",
                        rootdir.display()
                    )
                });
                process::exit(status);
            }
            Ok(what) => {
                tempdir.close().unwrap_or_else(|_| {
                    panic!(
                        "failed to remove temporary directory: {}",
                        rootdir.display()
                    )
                });
                eprintln!("unexpected wait event happend: {:?}", what);
                process::exit(1);
            }
            Err(e) => {
                tempdir.close().unwrap_or_else(|_| {
                    panic!(
                        "failed to remove temporary directory: {}",
                        rootdir.display()
                    )
                });
                eprintln!("waitpid failed: {}", e);
                process::exit(1);
            }
        };
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <nixpath> <command>\n", args[0]);
        process::exit(1);
    }
    let tempdir = TempDir::new().expect("failed to create temporary directory for mount point");
    let rootdir = PathBuf::from(tempdir.path());

    let nixdir = fs::canonicalize(&args[1])
        .unwrap_or_else(|_| panic!("failed to resolve nix directory {}", &args[1]));

    match unsafe { fork() } {
        Ok(ForkResult::Parent { child, .. }) => wait_for_child(child, tempdir, &rootdir),
        Ok(ForkResult::Child) => RunChroot::new(&rootdir).run_chroot(&nixdir, &args[2], &args[3..]),
        Err(e) => {
            eprintln!("fork failed: {}", e);
        }
    };
}
