//! gxwi-file-dialog: a dialog for choosing a file to open, or a place and a
//! name to save to, on a GXWI desktop, and what a program needs in order to
//! open it.
//!
//! The dialog is a program of its own, which a program that wants a file
//! chosen starts as a child of its own. The two speak over the child's
//! standard input and output, one JSON object a line (PSPU §9):
//!
//!   program → dialog   the first line: a [`Request`]
//!   dialog → program   { "type": "chosen", "path" }   once, then it ends
//!                   or { "type": "cancelled" }
//!
//! The dialog shows the person's folders as they may see them, and lets
//! them make a folder, rename and delete, as themselves: a small file
//! manager. It never opens the chosen file: the program does that, with the
//! path it is sent, and so never hands the dialog anything it holds.
//!
//! [`choose`] starts it and speaks for the program. This library has nothing
//! of GXWI's in it: with `default-features = false` it is the protocol alone.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// Where the dialog is installed.
pub const PROGRAM: &str = "/usr/bin/gxwi-file-dialog";

/// What the program tells the dialog, as the first line of its input.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub mode: Mode,
    /// What the dialog is called on its strip: "Export a key".
    pub title: String,
    /// What the file is for, for the person to read: "The key and
    /// everything under it is written to a registry document."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// The folder it opens on. Left out, or not there, the person's home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// The name it suggests, for saving: "Services.json".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The kinds of file it offers, the first chosen at first. Left empty,
    /// every file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Filter>,
}

/// Whether a file is to be opened or saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// An existing file, to be read.
    Open,
    /// A place and a name, to be written. Choosing a file that is there
    /// already asks the person first.
    Save,
}

/// A kind of file, by its name and the extensions it has.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    /// "Registry documents".
    pub name: String,
    /// Without the dot: `["json"]`.
    pub extensions: Vec<String>,
}

impl Filter {
    /// Whether `name` is of this kind, by its extension, without regard to
    /// case.
    pub fn takes(&self, name: &str) -> bool {
        let Some((_, extension)) = name.rsplit_once('.') else { return false };
        self.extensions.iter().any(|wanted| wanted.eq_ignore_ascii_case(extension))
    }
}

/// What the dialog tells the program, once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    /// The file chosen, as an absolute path.
    Chosen { path: PathBuf },
    /// The person cancelled or closed the dialog.
    Cancelled,
}

/// One line of the protocol: `message` as JSON, and the newline.
pub fn line(message: &impl Serialize) -> String {
    let mut line = serde_json::to_string(message).expect("the protocol's messages are all JSON");
    line.push('\n');
    line
}

/// Opens the dialog at [`PROGRAM`] on `request`, which is [`choose_with`].
pub fn choose(request: &Request, chosen: impl FnOnce(Option<PathBuf>) + Send + 'static) -> io::Result<()> {
    choose_with(Path::new(PROGRAM), request, chosen)
}

/// Starts `program` as the dialog on `request`, and waits on a thread of its
/// own for the answer: `chosen` is called once, with the path chosen, or
/// `None` if the person cancelled, or the dialog went without answering.
/// The dialog is the caller's child, and goes when the caller does.
pub fn choose_with(program: &Path, request: &Request, chosen: impl FnOnce(Option<PathBuf>) + Send + 'static) -> io::Result<()> {
    let mut child = Command::new(program).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let (Some(mut input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(io::Error::other("the dialog was started without its input and output"));
    };
    if let Err(e) = input.write_all(line(request).as_bytes()).and_then(|()| input.flush()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    std::thread::spawn(move || {
        let mut answer = None;
        for said in BufReader::new(output).lines() {
            let Ok(said) = said else { break };
            // A line this does not know is passed over: a newer dialog may
            // say more than this knows of.
            match serde_json::from_str(&said) {
                Ok(Answer::Chosen { path }) => {
                    answer = Some(path);
                    break;
                }
                Ok(Answer::Cancelled) => break,
                Err(_) => {}
            }
        }
        // The dialog's input stays open until it has answered, and it ends on
        // its own then.
        drop(input);
        let _ = child.wait();
        chosen(answer);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn the_messages_are_the_protocol_s_lines() {
        let request = Request {
            mode: Mode::Save,
            title: "Export a key".into(),
            purpose: None,
            folder: Some("/home/dana".into()),
            name: Some("Services.json".into()),
            filters: vec![Filter { name: "Registry documents".into(), extensions: vec!["json".into()] }],
        };
        assert_eq!(
            line(&request),
            "{\"mode\":\"save\",\"title\":\"Export a key\",\"folder\":\"/home/dana\",\"name\":\"Services.json\",\"filters\":[{\"name\":\"Registry documents\",\"extensions\":[\"json\"]}]}\n"
        );
        assert_eq!(line(&Answer::Chosen { path: "/home/dana/a.json".into() }), "{\"type\":\"chosen\",\"path\":\"/home/dana/a.json\"}\n");
        assert_eq!(line(&Answer::Cancelled), "{\"type\":\"cancelled\"}\n");
        // What a requester leaves out, it means the default of.
        let bare: Request = serde_json::from_str(r#"{"mode":"open","title":"Open"}"#).unwrap();
        assert_eq!((bare.folder, bare.name, bare.filters.len()), (None, None, 0));
    }

    #[test]
    fn a_filter_takes_names_by_their_extension() {
        let filter = Filter { name: "Documents".into(), extensions: vec!["json".into(), "reg".into()] };
        assert!(filter.takes("a.JSON") && filter.takes("x.y.reg"));
        assert!(!filter.takes("json") && !filter.takes("a.json.bak"));
    }

    /// A dialog that answers as `script` does: a shell script standing in
    /// for the program.
    fn answered(script: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("gxwi-file-dialog-test-{}-{}", std::process::id(), script.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("dialog");
        std::fs::write(&program, format!("#!/bin/sh\nread request\n{script}\n")).unwrap();
        std::fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let (tx, rx) = mpsc::channel();
        let request = Request { mode: Mode::Open, title: "Open".into(), purpose: None, folder: None, name: None, filters: Vec::new() };
        choose_with(&program, &request, move |chosen| tx.send(chosen).unwrap()).unwrap();
        let answer = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        answer
    }

    #[test]
    fn the_answer_comes_back_to_the_program() {
        assert_eq!(answered(r#"echo '{"type":"something new"}'; echo '{"type":"chosen","path":"/tmp/x"}'"#), Some(PathBuf::from("/tmp/x")));
        assert_eq!(answered(r#"echo '{"type":"cancelled"}'"#), None);
        // A dialog that goes without a word is as good as cancelled.
        assert_eq!(answered("exit 3"), None);
    }
}
