//! gxwi-file-dialog — a dialog for choosing a file to open or a place to
//! save to, on a GXWI desktop, opened by a program that wants one (the
//! library, and PSPU §9, set out how the two speak).
//!
//! It shows one folder at a time: its folders, then its files, of the kind
//! the program asked for. The person goes into a folder by opening it, up
//! with Up, home with Home, or anywhere by typing a path. To open, they pick
//! a file; to save, they type a name, and saving over a file that is there
//! asks first. They can make a folder, and rename and delete what they
//! pick, as themselves: what they may not do is offered disabled, with why.
//! The dialog answers once, with the path or that it was cancelled, and
//! ends. It never opens the file: the program does.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gxwi_file_dialog::{Answer, Mode, Request, line};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use libgxwi::{App, Closer, Facts, Fields, Live, Value, escape};

mod folder;

use folder::{Entry, May};

// What this program looks like on its dialog's strip. The icon itself is
// `gxwi-file-dialog.svg`, installed as the base theme's.
libgxwi::icon!(b"dev.peios.gxwi-file-dialog");

/// What the dialog is doing besides showing the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Doing {
    Looking,
    /// A name for a new folder, being typed.
    NewFolder,
    /// A new name for what is picked, being typed.
    Renaming(String),
    /// Asking before what is picked is deleted.
    Deleting(String),
    /// Asking before a file that is there is saved over.
    Overwriting(PathBuf),
}

struct Chooser {
    request: Request,
    folder: PathBuf,
    entries: Result<Vec<Entry>, String>,
    may: May,
    picked: Option<String>,
    hidden: bool,
    /// Which of the program's kinds of file is shown, or `None` for all.
    filter: Option<usize>,
    doing: Doing,
    /// What came of the last thing done, for the person to read, and
    /// whether it went wrong.
    said: Option<(String, bool)>,
    zone: TimeZone,
    closer: Option<Closer>,
    answered: bool,
}

impl Chooser {
    fn new(request: Request) -> Chooser {
        let home = home();
        // The folder asked for, if it is one the person may list (PSPU §9.4).
        let folder = request.folder.clone().filter(|folder| folder.is_absolute() && folder::list(folder, false).is_ok()).unwrap_or(home);
        let filter = (!request.filters.is_empty()).then_some(0);
        let mut chooser = Chooser {
            request,
            folder: PathBuf::new(),
            entries: Ok(Vec::new()),
            may: May::UNTOLD,
            picked: None,
            hidden: false,
            filter,
            doing: Doing::Looking,
            said: None,
            zone: TimeZone::system(),
            closer: None,
            answered: false,
        };
        chooser.go(&folder);
        chooser
    }

    /// Shows the folder at `path`.
    fn go(&mut self, path: &Path) {
        self.folder = path.to_path_buf();
        self.read();
        self.picked = None;
        self.doing = Doing::Looking;
    }

    /// Reads the folder shown again.
    fn read(&mut self) {
        self.entries = folder::list(&self.folder, self.hidden);
        self.may = folder::may(&self.folder);
    }

    /// Whether a file of this name is shown: in a dialog for opening, only
    /// files of the kind chosen.
    fn shows(&self, entry: &Entry) -> bool {
        entry.folder || self.request.mode == Mode::Save || self.filter.is_none_or(|at| self.request.filters[at].takes(&entry.name))
    }

    fn entry(&self, name: &str) -> Option<&Entry> {
        self.entries.as_ref().ok()?.iter().find(|entry| entry.name == name)
    }

    /// Tells the program what was chosen, once, and goes.
    fn answer(&mut self, answer: Answer) {
        if self.answered {
            return;
        }
        self.answered = true;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(line(&answer).as_bytes()).and_then(|()| out.flush());
        if let Some(closer) = &self.closer {
            closer.close();
        }
    }

    /// Opens what is named `name` in the folder: a folder is gone into; a
    /// file is chosen, or for saving, named.
    fn open(&mut self, name: &str, fields: &mut Fields) {
        let Some(entry) = self.entry(name).cloned() else { return };
        let path = self.folder.join(&entry.name);
        if entry.folder {
            fields.set("path", &path.to_string_lossy());
            self.go(&path);
        } else if self.request.mode == Mode::Open {
            self.answer(Answer::Chosen { path });
        } else {
            fields.set("name", &entry.name);
            self.save(fields);
        }
    }

    /// Chooses, as the footer's button does.
    fn choose(&mut self, fields: &mut Fields) {
        match self.request.mode {
            Mode::Open => match self.picked.clone() {
                Some(name) => self.open(&name, fields),
                None => self.said = Some(("Pick a file to open.".into(), true)),
            },
            Mode::Save => self.save(fields),
        }
    }

    /// Saves to the name typed, in the folder shown: going into it if it
    /// names a folder, asking first if it names a file that is there.
    fn save(&mut self, fields: &mut Fields) {
        let typed = match folder::name(fields.get("name")) {
            Ok(typed) => typed.to_string(),
            Err(why) => return self.said = Some((why, true)),
        };
        // The name of a folder here goes into it.
        if self.folder.join(&typed).is_dir() {
            let path = self.folder.join(&typed);
            fields.set("path", &path.to_string_lossy());
            return self.go(&path);
        }
        // A name without an extension takes the kind chosen's.
        let named = match self.filter.map(|at| &self.request.filters[at]) {
            Some(filter) if !typed.contains('.') && !filter.extensions.is_empty() => format!("{typed}.{}", filter.extensions[0]),
            _ => typed,
        };
        let path = self.folder.join(&named);
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                fields.set("path", &path.to_string_lossy());
                self.go(&path);
            }
            Ok(_) => self.doing = Doing::Overwriting(path),
            Err(_) if !self.may.add_file => self.said = Some(("You may not save files in this folder.".into(), true)),
            Err(_) => self.answer(Answer::Chosen { path }),
        }
    }

    /// What the name field's form does: make a folder, or rename.
    fn named(&mut self, fields: &mut Fields) {
        let typed = match folder::name(fields.get("new-name")) {
            Ok(typed) => typed.to_string(),
            Err(why) => return self.said = Some((why, true)),
        };
        let done = match self.doing.clone() {
            Doing::NewFolder => folder::make_folder(&self.folder, &typed).map(|_| format!("Made the folder {typed}.")),
            Doing::Renaming(was) => folder::rename(&self.folder, &was, &typed).map(|()| format!("Renamed {was} to {typed}.")),
            _ => return,
        };
        match done {
            Ok(said) => {
                self.said = Some((said, false));
                self.doing = Doing::Looking;
                self.read();
                self.picked = Some(typed);
            }
            Err(why) => self.said = Some((why, true)),
        }
    }

    fn row(&self, entry: &Entry) -> String {
        let changed = entry
            .changed
            .and_then(|at| Timestamp::try_from(at).ok())
            .map(|at| at.to_zoned(self.zone.clone()).strftime("%-d %b %Y, %H:%M").to_string())
            .unwrap_or_default();
        format!(
            "<li><button type=\"button\" class=\"{kind}\" fx-click=\"pick\" fx-dblclick=\"open\" fx-value-name=\"{name}\" aria-selected=\"{picked}\">\
             <span class=\"icon\" aria-hidden=\"true\"></span><span class=\"name\">{name}</span>\
             <span class=\"size\">{size}</span><span class=\"changed\">{changed}</span></button></li>",
            kind = if entry.folder { "folder" } else { "file" },
            name = escape(&entry.name),
            picked = self.picked.as_deref() == Some(entry.name.as_str()),
            size = if entry.folder { String::new() } else { folder::size(entry.size) },
            changed = escape(&changed),
        )
    }

    /// The tools for the folder and what is picked, offered as far as the
    /// person may use them.
    fn tools(&self) -> String {
        let off = |allowed: bool, why: &str| if allowed { String::new() } else { format!(" disabled title=\"{}\"", escape(why)) };
        let picked = self.picked.as_deref().filter(|name| self.entry(name).is_some());
        let may_delete = picked.is_some_and(|name| folder::may_delete(&self.folder.join(name), self.may));
        let pick_first = |allowed: bool, why: &str| if picked.is_none() { " disabled title=\"Pick something first.\"".to_string() } else { off(allowed, why) };
        format!(
            "<div class=\"tools\">\
             <button type=\"button\" fx-click=\"new-folder\"{new}>New folder…</button>\
             <button type=\"button\" fx-click=\"rename\"{rename}>Rename…</button>\
             <button type=\"button\" fx-click=\"delete\"{delete}>Delete…</button>\
             <button type=\"button\" class=\"hidden-toggle\" fx-click=\"hidden\" aria-pressed=\"{hidden}\">Hidden files</button></div>",
            new = match &self.entries {
                Err(_) => off(false, "Nothing can be made in a folder that can't be listed."),
                Ok(_) => off(self.may.add_folder, "You may not make folders here."),
            },
            rename = pick_first(may_delete && self.may.add_file, "You may not rename this."),
            delete = pick_first(may_delete, "You may not delete this."),
            hidden = self.hidden,
        )
    }

    /// The strip for what is being done: a name being typed, or a question.
    fn doing(&self) -> String {
        match &self.doing {
            Doing::Looking => String::new(),
            Doing::NewFolder | Doing::Renaming(_) => format!(
                "<form class=\"strip\" fx-submit=\"named\"><label>{}<input name=\"new-name\" autocomplete=\"off\" spellcheck=\"false\" fx-autofocus></label>\
                 <button type=\"submit\" class=\"primary\">{}</button><button type=\"button\" fx-click=\"back\">Cancel</button></form>",
                if self.doing == Doing::NewFolder { "New folder" } else { "New name" },
                if self.doing == Doing::NewFolder { "Make" } else { "Rename" },
            ),
            Doing::Deleting(name) => {
                let what = if self.entry(name).is_some_and(|entry| entry.folder) { "the folder" } else { "" };
                let under = if what.is_empty() { "" } else { " and everything in it" };
                format!(
                    "<div class=\"strip asking\"><p>Delete {what} <strong>{name}</strong>{under}? This can't be undone.</p>\
                     <button type=\"button\" class=\"danger\" fx-click=\"delete-yes\" fx-autofocus>Delete</button><button type=\"button\" fx-click=\"back\">Cancel</button></div>",
                    name = escape(name),
                )
            }
            Doing::Overwriting(path) => format!(
                "<div class=\"strip asking\"><p><strong>{}</strong> is here already. Save over it?</p>\
                 <button type=\"button\" class=\"danger\" fx-click=\"overwrite-yes\" fx-autofocus>Save over it</button><button type=\"button\" fx-click=\"back\">Cancel</button></div>",
                escape(&path.file_name().unwrap_or_default().to_string_lossy()),
            ),
        }
    }
}

impl Live for Chooser {
    fn render(&self, _: &Facts) -> String {
        let purpose = self.request.purpose.as_ref().map(|purpose| format!("<p>{}</p>", escape(purpose))).unwrap_or_default();
        let said = self
            .said
            .as_ref()
            .map(|(text, bad)| format!("<p class=\"said{}\" role=\"status\">{}</p>", if *bad { " bad" } else { "" }, escape(text)))
            .unwrap_or_default();
        let listing = match &self.entries {
            Ok(entries) => {
                let rows: String = entries.iter().filter(|entry| self.shows(entry)).map(|entry| self.row(entry)).collect();
                if rows.is_empty() {
                    "<p class=\"empty\">Nothing here.</p>".to_string()
                } else {
                    format!("<ul class=\"entries\" role=\"listbox\">{rows}</ul>")
                }
            }
            Err(why) => format!("<p class=\"empty bad\">{}</p>", escape(why)),
        };
        let filters = if self.request.filters.is_empty() {
            String::new()
        } else {
            let mut options: String = self
                .request
                .filters
                .iter()
                .enumerate()
                .map(|(at, filter)| {
                    let extensions = filter.extensions.iter().map(|e| format!("*.{e}")).collect::<Vec<_>>().join(" ");
                    format!("<option value=\"{at}\">{} ({})</option>", escape(&filter.name), escape(&extensions))
                })
                .collect();
            options += "<option value=\"all\">All files</option>";
            format!("<select name=\"filter\" aria-label=\"Kind of file\">{options}</select>")
        };
        let (name, button) = match self.request.mode {
            Mode::Open => (String::new(), "Open"),
            Mode::Save => (
                "<label class=\"name\">Name<input name=\"name\" autocomplete=\"off\" spellcheck=\"false\" fx-autofocus></label>".to_string(),
                "Save",
            ),
        };
        let escape_key = "<div hidden><button type=\"button\" fx-key=\"Escape\" fx-click=\"cancel\"></button></div>";
        format!(
            "{escape_key}<div class=\"chooser\" fx-fit><header><h1>{title}</h1>{purpose}</header>\
             <form class=\"where\" fx-submit=\"go\">\
             <button type=\"button\" fx-click=\"up\" title=\"The folder above\">Up</button>\
             <button type=\"button\" fx-click=\"home\">Home</button>\
             <input name=\"path\" autocomplete=\"off\" spellcheck=\"false\" aria-label=\"The folder's path\"><button type=\"submit\">Go</button></form>\
             {tools}{doing}{said}{listing}\
             <form class=\"choose\" fx-submit=\"choose\">{name}{filters}\
             <span class=\"buttons\"><button type=\"button\" fx-click=\"cancel\">Cancel</button><button type=\"submit\" class=\"primary\">{button}</button></span></form></div>",
            title = escape(&self.request.title),
            tools = self.tools(),
            doing = self.doing(),
        )
    }

    fn event(&mut self, name: &str, value: &Value, fields: &mut Fields) {
        let named = value["name"].as_str().map(str::to_string);
        if !matches!(name, "named" | "delete-yes") {
            self.said = None;
        }
        match name {
            "go" => {
                let typed = fields.get("path").trim().to_string();
                let path = if typed.starts_with('/') { PathBuf::from(typed) } else { self.folder.join(typed) };
                self.go(&path);
            }
            "up" => {
                if let Some(above) = self.folder.parent().map(Path::to_path_buf) {
                    self.go(&above);
                }
            }
            "home" => self.go(&home()),
            "pick" => {
                if let Some(picked) = named {
                    if self.request.mode == Mode::Save && self.entry(&picked).is_some_and(|entry| !entry.folder) {
                        fields.set("name", &picked);
                    }
                    self.picked = Some(picked);
                }
            }
            "open" => {
                if let Some(picked) = named {
                    self.open(&picked, fields);
                }
            }
            "choose" => self.choose(fields),
            "overwrite-yes" => {
                if let Doing::Overwriting(path) = self.doing.clone() {
                    self.answer(Answer::Chosen { path });
                }
            }
            "new-folder" if self.may.add_folder => {
                fields.set("new-name", "New folder");
                self.doing = Doing::NewFolder;
            }
            "rename" => {
                if let Some(picked) = self.picked.clone() {
                    fields.set("new-name", &picked);
                    self.doing = Doing::Renaming(picked);
                }
            }
            "named" => self.named(fields),
            "delete" => {
                if let Some(picked) = self.picked.clone() {
                    self.doing = Doing::Deleting(picked);
                }
            }
            "delete-yes" => {
                if let Doing::Deleting(picked) = self.doing.clone() {
                    self.said = Some(match folder::delete(&self.folder.join(&picked)) {
                        Ok(()) => (format!("Deleted {picked}."), false),
                        Err(why) => (why, true),
                    });
                    self.doing = Doing::Looking;
                    self.picked = None;
                    self.read();
                }
            }
            "hidden" => {
                self.hidden = !self.hidden;
                self.read();
            }
            "back" => self.doing = Doing::Looking,
            // Esc, or Cancel: what is being done is let go first, and then
            // the dialog.
            "cancel" if self.doing != Doing::Looking => self.doing = Doing::Looking,
            "cancel" => self.answer(Answer::Cancelled),
            _ => {}
        }
        if matches!(name, "go" | "up" | "home" | "open") {
            fields.set("path", &self.folder.to_string_lossy());
        }
    }

    fn input(&mut self, name: &str, fields: &mut Fields) {
        if name == "filter" {
            self.filter = fields.get("filter").parse().ok().filter(|at| *at < self.request.filters.len());
        }
    }

    fn closing(&mut self, _: &mut Fields) -> bool {
        self.answer(Answer::Cancelled);
        true
    }
}

/// The person's home folder.
fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).filter(|home| home.is_dir()).unwrap_or_else(|| PathBuf::from("/"))
}

/// The request, from the first line of the input.
fn request() -> Result<Request, String> {
    let mut first = String::new();
    std::io::stdin().lock().read_line(&mut first).map_err(|e| format!("the request could not be read: {e}"))?;
    if first.trim().is_empty() {
        return Err("no request: it is opened by a program, which says what is wanted on its first line (see gxwi-file-dialog(1))".into());
    }
    serde_json::from_str(&first).map_err(|e| format!("the request is not one: {e}"))
}

fn main() {
    // Gone with whatever opened it, however that went.
    // SAFETY: prctl with these arguments only sets a signal for later.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
    if unsafe { libc::getppid() } == 1 {
        std::process::exit(0);
    }
    let request = request().unwrap_or_else(|why| die(&why));
    let title = request.title.clone();
    let chooser = Chooser::new(request);
    let mut app = App::connect().unwrap_or_else(|e| die(&format!("no desktop to open on: {e}")));
    app.stylesheet("/dialog.css", include_str!("dialog.css"));
    let dialog = app.dialog(&title, chooser);
    let closer = dialog.closer();
    dialog.update(|chooser, fields| {
        chooser.closer = Some(closer);
        fields.set("path", &chooser.folder.to_string_lossy());
        if let Some(name) = &chooser.request.name {
            fields.set("name", name);
        }
        fields.set("filter", if chooser.filter.is_some() { "0" } else { "all" });
    });
    // The program going: so does the dialog, unanswered.
    let watched = Arc::clone(&dialog);
    std::thread::spawn(move || {
        for said in std::io::stdin().lock().lines() {
            if said.is_err() {
                break;
            }
        }
        watched.close();
        std::process::exit(0);
    });
    if let Err(e) = app.run() {
        die(&e.to_string());
    }
}

fn die(why: &str) -> ! {
    eprintln!("gxwi-file-dialog: {why}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gxwi_file_dialog::Filter;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gxwi-file-dialog-main-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Sub")).unwrap();
        std::fs::write(dir.join("a.json"), "{}").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        dir
    }

    fn chooser(mode: Mode, dir: &Path) -> Chooser {
        let mut chooser = Chooser::new(Request {
            mode,
            title: "Choose".into(),
            purpose: Some("For a test.".into()),
            folder: Some(dir.to_path_buf()),
            name: None,
            filters: vec![Filter { name: "Documents".into(), extensions: vec!["json".into()] }],
        });
        // No writing to the test's own output.
        chooser.answered = false;
        chooser
    }

    fn shown(chooser: &Chooser) -> String {
        chooser.render(&Facts { views: 1, fields: &Fields::default() })
    }

    #[test]
    fn to_open_it_shows_folders_and_files_of_the_kind_chosen() {
        let dir = scratch("open");
        let mut chooser = chooser(Mode::Open, &dir);
        let html = shown(&chooser);
        assert!(html.contains("fx-value-name=\"Sub\"") && html.contains("fx-value-name=\"a.json\""));
        assert!(!html.contains("notes.txt"));
        let mut fields = Fields::default();
        fields.set("filter", "all");
        chooser.input("filter", &mut fields);
        assert!(shown(&chooser).contains("notes.txt"));
        // Opening a folder goes into it.
        chooser.open("Sub", &mut fields);
        assert_eq!(chooser.folder, dir.join("Sub"));
        assert_eq!(fields.get("path"), dir.join("Sub").to_string_lossy());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn to_save_a_name_takes_the_kind_s_extension_and_saving_over_a_file_asks_first() {
        let dir = scratch("save");
        let mut chooser = chooser(Mode::Save, &dir);
        let mut fields = Fields::default();
        fields.set("name", "a");
        chooser.answered = true; // nothing is written out
        chooser.save(&mut fields);
        assert_eq!(chooser.doing, Doing::Overwriting(dir.join("a.json")));
        assert!(shown(&chooser).contains("<strong>a.json</strong> is here already. Save over it?"));
        // A name for a folder goes into it.
        fields.set("name", "Sub");
        chooser.doing = Doing::Looking;
        chooser.save(&mut fields);
        assert_eq!(chooser.folder, dir.join("Sub"));
        fields.set("name", "../x");
        chooser.save(&mut fields);
        assert!(chooser.said.as_ref().is_some_and(|(said, bad)| *bad && said.contains("can't be")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn folders_are_made_renamed_and_deleted_after_asking() {
        let dir = scratch("acts");
        let mut chooser = chooser(Mode::Open, &dir);
        let mut fields = Fields::default();
        chooser.event("new-folder", &serde_json::json!({}), &mut fields);
        assert_eq!(fields.get("new-name"), "New folder");
        fields.set("new-name", "Made");
        chooser.event("named", &serde_json::json!({}), &mut fields);
        assert!(dir.join("Made").is_dir());
        assert_eq!(chooser.picked.as_deref(), Some("Made"));
        chooser.event("rename", &serde_json::json!({}), &mut fields);
        fields.set("new-name", "Renamed");
        chooser.event("named", &serde_json::json!({}), &mut fields);
        assert!(dir.join("Renamed").is_dir());
        chooser.event("delete", &serde_json::json!({}), &mut fields);
        assert!(shown(&chooser).contains("Delete the folder <strong>Renamed</strong> and everything in it?"));
        chooser.event("delete-yes", &serde_json::json!({}), &mut fields);
        assert!(!dir.join("Renamed").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_folder_that_may_not_be_listed_says_so() {
        let dir = scratch("gone");
        let mut chooser = chooser(Mode::Open, &dir);
        chooser.go(&dir.join("nowhere"));
        let html = shown(&chooser);
        assert!(html.contains("<p class=\"empty bad\">There is no folder here.</p>"));
        assert!(html.contains("fx-click=\"new-folder\" disabled title=\"Nothing can be made in a folder that can't be listed.\""));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nothing_picked_leaves_rename_and_delete_waiting() {
        let dir = scratch("tools");
        let chooser = chooser(Mode::Open, &dir);
        assert!(shown(&chooser).contains("fx-click=\"delete\" disabled title=\"Pick something first.\""));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
