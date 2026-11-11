use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Seconds since the Unix epoch, the unit of `started` and `ended`.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Ran,
    Declined,
    Killed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub id: String,
    pub command: String,
    pub exit_code: i32,
    pub status: Status,
    pub started: u64,
    pub ended: u64,
    pub output: String,
}

impl RunResult {
    pub fn to_json(&self) -> String {
        let status = match self.status {
            Status::Ran => "ran",
            Status::Declined => "declined",
            Status::Killed => "killed",
        };
        format!(
            "{{\"id\":{},\"command\":{},\"exit_code\":{},\"status\":{},\"started\":{},\"ended\":{},\"output\":{}}}",
            escape(&self.id),
            escape(&self.command),
            self.exit_code,
            escape(status),
            self.started,
            self.ended,
            escape(&self.output)
        )
    }

    pub fn write_output_file(&self, path: &str) -> Result<(), String> {
        use std::os::unix::fs::OpenOptionsExt;
        // Created 0600 from the start: the output can hold secrets, so it is never
        // world-readable, not even for the moment before a chmod.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("{}: {}", path, e))?;
        file.write_all(self.output.as_bytes())
            .map_err(|e| format!("{}: {}", path, e))?;
        // An existing file keeps its old mode on truncate, so set it explicitly too.
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perms).map_err(|e| format!("{}: {}", path, e))
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn temp_path(name: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "{}-{}-{}",
            std::process::id(),
            name,
            std::process::id()
        ));
        p.display().to_string()
    }

    #[test]
    fn normal_ran_result() {
        let r = RunResult {
            id: "run-1".to_string(),
            command: "echo hi".to_string(),
            exit_code: 0,
            status: Status::Ran,
            started: 1_700_000_000,
            ended: 1_700_000_001,
            output: "hi\n".to_string(),
        };
        let json = r.to_json();
        assert_eq!(
            json,
            "{\"id\":\"run-1\",\"command\":\"echo hi\",\"exit_code\":0,\"status\":\"ran\",\"started\":1700000000,\"ended\":1700000001,\"output\":\"hi\\n\"}"
        );
        let path = temp_path("normal");
        r.write_output_file(&path).unwrap();
        let data = fs::read_to_string(&path).unwrap();
        assert_eq!(data, "hi\n");
        let meta = fs::metadata(&path).unwrap();
        #[cfg(unix)]
        {
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn escaping_edge_cases() {
        let r = RunResult {
            id: "esc".to_string(),
            command: "echo \"a\\b\tc\"".to_string(),
            exit_code: 2,
            status: Status::Ran,
            started: 10,
            ended: 11,
            output: "quote\" back\\slash\t tab \u{1} end".to_string(),
        };
        let json = r.to_json();
        assert!(json.contains("\"command\":\"echo \\\"a\\\\b\\tc\\\"\""));
        assert!(json.contains("\"output\":\"quote\\\" back\\\\slash\\t tab \\u0001 end\""));
        // empty output
        let empty = RunResult {
            id: "e".to_string(),
            command: "x".to_string(),
            exit_code: 0,
            status: Status::Ran,
            started: 1,
            ended: 2,
            output: String::new(),
        };
        assert!(empty.to_json().contains("\"output\":\"\""));
        // non-ascii preserved
        let acc = RunResult {
            id: "a".to_string(),
            command: "y".to_string(),
            exit_code: 0,
            status: Status::Ran,
            started: 1,
            ended: 2,
            output: "é".to_string(),
        };
        assert!(acc.to_json().contains("\"output\":\"é\""));
        // large round-trip
        let big = "x".repeat(200_000);
        let big_r = RunResult {
            id: "big".to_string(),
            command: "b".to_string(),
            exit_code: 0,
            status: Status::Ran,
            started: 1,
            ended: 2,
            output: big.clone(),
        };
        let path = temp_path("big");
        big_r.write_output_file(&path).unwrap();
        let data = fs::read_to_string(&path).unwrap();
        assert_eq!(data.len(), 200_000);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn declined_and_killed_status() {
        let d = RunResult {
            id: "d".to_string(),
            command: "c".to_string(),
            exit_code: 1,
            status: Status::Declined,
            started: 1,
            ended: 2,
            output: "no".to_string(),
        };
        assert!(d.to_json().contains("\"status\":\"declined\""));
        let k = RunResult {
            id: "k".to_string(),
            command: "c".to_string(),
            exit_code: 137,
            status: Status::Killed,
            started: 1,
            ended: 2,
            output: "no".to_string(),
        };
        assert!(k.to_json().contains("\"status\":\"killed\""));
    }

    #[test]
    fn nonexistent_directory() {
        let r = RunResult {
            id: "n".to_string(),
            command: "c".to_string(),
            exit_code: 0,
            status: Status::Ran,
            started: 1,
            ended: 2,
            output: "x".to_string(),
        };
        let bad = "/no/such/dir/xyz.txt";
        let err = r.write_output_file(bad).unwrap_err();
        assert!(err.contains(bad));
    }
}
