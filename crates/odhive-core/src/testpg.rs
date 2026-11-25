//! Test helper: a throwaway PostgreSQL server.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::db::PgTarget;

/// A throwaway PostgreSQL server listening only on a Unix socket in a
/// temporary folder. `None` (test skipped) when no server binaries exist,
/// unless `ODHIVE_REQUIRE_PG` is set (CI), which makes that a failure.
pub(crate) struct TempPg {
    child: Child,
    pub(crate) socket_dir: PathBuf,
    _dir: tempfile::TempDir,
}

impl TempPg {
    fn bin_dir() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        if let Some(initdb) = crate::python::find_executable(&path, "initdb") {
            return initdb.parent().map(Path::to_path_buf);
        }
        // Debian/Ubuntu keep the server off PATH.
        let mut dirs: Vec<PathBuf> = fs::read_dir("/usr/lib/postgresql")
            .ok()?
            .flatten()
            .map(|e| e.path().join("bin"))
            .filter(|b| b.join("initdb").is_file())
            .collect();
        dirs.sort();
        dirs.pop()
    }

    pub(crate) fn start() -> Option<Self> {
        let Some(bin) = Self::bin_dir() else {
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
        // `pw` must use a password; everyone else is trusted.
        fs::write(
            data.join("pg_hba.conf"),
            "local all pw scram-sha-256\nlocal all all trust\n",
        )
        .expect("pg_hba");
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
            _dir: dir,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !pg.socket_dir.join(".s.PGSQL.5432").exists() {
            assert!(Instant::now() < deadline, "postgres did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(pg)
    }

    pub(crate) fn target(&self, user: Option<&str>) -> PgTarget {
        PgTarget {
            socket_dirs: vec![self.socket_dir.clone()],
            port: 5432,
            user: user.map(str::to_owned),
            ..PgTarget::nowhere()
        }
    }

    pub(crate) async fn sql(&self, sql: &str) {
        let (client, conn) = tokio_postgres::Config::new()
            .host_path(&self.socket_dir)
            .port(5432)
            .dbname("postgres")
            .connect(tokio_postgres::NoTls)
            .await
            .expect("connect");
        tokio::spawn(conn);
        client.batch_execute(sql).await.expect("sql");
    }
}

impl Drop for TempPg {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
