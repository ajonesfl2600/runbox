#[derive(Debug)]
pub struct Req {
    pub command: String,
    pub id: Option<String>,
    pub json: bool,
    pub output_file: Option<String>,
}

#[derive(Debug)]
pub enum Invocation {
    Open,
    List,
    Help,
    Request(Req),
    Bad(String),
}

fn valid_id(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=32).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'.' || *c == b'_' || *c == b'-')
}

pub fn parse(args: &[String]) -> Invocation {
    if args.is_empty() {
        return Invocation::Open;
    }
    if args.len() == 1 {
        match args[0].as_str() {
            "--help" | "-h" => return Invocation::Help,
            "--list" => return Invocation::List,
            _ => {}
        }
    }

    let mut command: Option<String> = None;
    let mut id: Option<String> = None;
    let mut json = false;
    let mut output_file: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--request" => {
                if command.is_some() || i + 1 >= args.len() {
                    return Invocation::Bad(args.join(" "));
                }
                let cmd = args[i + 1].clone();
                if cmd.trim().is_empty() {
                    return Invocation::Bad(args.join(" "));
                }
                command = Some(cmd);
                i += 2;
            }
            "--id" => {
                if id.is_some() || i + 1 >= args.len() {
                    return Invocation::Bad(args.join(" "));
                }
                let name = args[i + 1].clone();
                if !valid_id(&name) {
                    return Invocation::Bad(args.join(" "));
                }
                id = Some(name);
                i += 2;
            }
            "--json" => {
                if json {
                    return Invocation::Bad(args.join(" "));
                }
                json = true;
                i += 1;
            }
            "--output-file" => {
                if output_file.is_some() || i + 1 >= args.len() {
                    return Invocation::Bad(args.join(" "));
                }
                output_file = Some(args[i + 1].clone());
                i += 2;
            }
            _ => return Invocation::Bad(args.join(" ")),
        }
    }

    match command {
        Some(cmd) => Invocation::Request(Req {
            command: cmd,
            id,
            json,
            output_file,
        }),
        None => Invocation::Bad(args.join(" ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn normal_full() {
        match parse(&a(&[
            "--request",
            "ls",
            "--id",
            "mac-wipe",
            "--json",
            "--output-file",
            "/tmp/o.txt",
        ])) {
            Invocation::Request(r) => {
                assert_eq!(r.command, "ls");
                assert_eq!(r.id, Some("mac-wipe".to_string()));
                assert!(r.json);
                assert_eq!(r.output_file, Some("/tmp/o.txt".to_string()));
            }
            o => panic!("expected Request, got {:?}", o),
        }
    }

    #[test]
    fn normal_json_first() {
        match parse(&a(&["--json", "--request", "ls"])) {
            Invocation::Request(r) => {
                assert_eq!(r.command, "ls");
                assert_eq!(r.id, None);
                assert!(r.json);
                assert_eq!(r.output_file, None);
            }
            o => panic!("expected Request, got {:?}", o),
        }
    }

    #[test]
    fn edge_32_id_ok() {
        let id = "a".repeat(32);
        match parse(&a(&["--request", "ls", "--id", &id])) {
            Invocation::Request(r) => assert_eq!(r.id, Some(id)),
            o => panic!("expected Request, got {:?}", o),
        }
    }

    #[test]
    fn edge_33_id_bad() {
        assert!(matches!(
            parse(&a(&["--request", "ls", "--id", &"a".repeat(33)])),
            Invocation::Bad(_)
        ));
    }

    #[test]
    fn edge_id_space_bad() {
        assert!(matches!(
            parse(&a(&["--request", "ls", "--id", "a b"])),
            Invocation::Bad(_)
        ));
    }

    #[test]
    fn neg_json_twice() {
        assert!(matches!(
            parse(&a(&["--request", "ls", "--json", "--json"])),
            Invocation::Bad(_)
        ));
    }

    #[test]
    fn neg_id_missing_value() {
        assert!(matches!(
            parse(&a(&["--request", "ls", "--id"])),
            Invocation::Bad(_)
        ));
    }

    #[test]
    fn neg_json_only() {
        assert!(matches!(parse(&a(&["--json"])), Invocation::Bad(_)));
    }

    #[test]
    fn neg_list_mixed() {
        assert!(matches!(parse(&a(&["--list", "x"])), Invocation::Bad(_)));
    }

    #[test]
    fn neg_bogus() {
        assert!(matches!(parse(&a(&["--bogus"])), Invocation::Bad(_)));
    }

    #[test]
    fn neg_ws_command() {
        assert!(matches!(
            parse(&a(&["--request", "  "])),
            Invocation::Bad(_)
        ));
    }

    #[test]
    fn open_help_list() {
        assert!(matches!(parse(&a(&[])), Invocation::Open));
        assert!(matches!(parse(&a(&["--help"])), Invocation::Help));
        assert!(matches!(parse(&a(&["-h"])), Invocation::Help));
        assert!(matches!(parse(&a(&["--list"])), Invocation::List));
    }
}
