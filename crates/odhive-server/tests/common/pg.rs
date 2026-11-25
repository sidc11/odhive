//! A throwaway PostgreSQL server for API tests (like `odhive_core`'s own
//! test helper): only a Unix socket in a temporary folder. `None` (test
//! skipped) when no server binaries exist, unless `ODHIVE_REQUIRE_PG` is set.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use odhive_core::python::find_executable;

pub struct TempPg {
    child: Child,
    pub socket_dir: PathBuf,
    pub bin: PathBuf,
    _dir: tempfile::TempDir,
}

fn bin_dir() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    if let Some(initdb) = find_executable(&path, "initdb") {
        return initdb.parent().map(Path::to_path_buf);
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir("/usr/lib/postgresql")
        .ok()?
        .flatten()
        .map(|e| e.path().join("bin"))
        .filter(|b| b.join("initdb").is_file())
        .collect();
    dirs.sort();
    dirs.pop()
}

impl TempPg {
    pub fn start() -> Option<Self> {
        let Some(bin) = bin_dir() else {
            assert!(
                std::env::var_os("ODHIVE_REQUIRE_PG").is_none_or(|v| v.is_empty()),
                "ODHIVE_REQUIRE_PG is set but no PostgreSQL server binaries were found"
            );
            return None;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("data");
        let socket_dir = dir.path().join("sock");
        fs::create_dir(&socket_dir).expect("mkdir");
        let out = Command::new(bin.join("initdb"))
            .args(["-A", "trust", "-E", "UTF8", "--no-instructions", "-D"])
            .arg(&data)
            .output()
            .expect("initdb");
        assert!(out.status.success(), "initdb: {out:?}");
        let child = Command::new(bin.join("postgres"))
            .arg("-D")
            .arg(&data)
            .arg("-k")
            .arg(&socket_dir)
            .args(["-c", "listen_addresses=", "-p", "5432"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("postgres");
        let pg = Self {
            child,
            socket_dir,
            bin,
            _dir: dir,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !pg.socket_dir.join(".s.PGSQL.5432").exists() {
            assert!(Instant::now() < deadline, "postgres did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(pg)
    }

    fn tool(&self, name: &str) -> Command {
        let mut cmd = Command::new(self.bin.join(name));
        cmd.env("PGHOST", &self.socket_dir).env("PGPORT", "5432");
        cmd
    }

    /// Runs `sql` in `db`; returns the rows, `|`-separated.
    pub fn psql(&self, db: &str, sql: &str) -> Vec<String> {
        let out = self
            .tool("psql")
            .args([
                "-X",
                "-q",
                "-At",
                "-v",
                "ON_ERROR_STOP=1",
                "-d",
                db,
                "-c",
                sql,
            ])
            .output()
            .expect("psql");
        assert!(out.status.success(), "psql: {out:?}");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// `pg_dump --no-owner --format=<format>` of `db` to `out`.
    pub fn dump(&self, db: &str, format: &str, out: &Path) {
        let done = self
            .tool("pg_dump")
            .args(["--no-owner", &format!("--format={format}"), "-f"])
            .arg(out)
            .arg(db)
            .output()
            .expect("pg_dump");
        assert!(done.status.success(), "pg_dump: {done:?}");
    }
}

impl Drop for TempPg {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
