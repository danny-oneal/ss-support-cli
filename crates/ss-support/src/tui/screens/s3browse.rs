//! A browser for S3: buckets, then prefixes and objects, then the versions
//! of one object, two of which can be marked and compared.

use std::cell::Cell;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ss_core::ops::s3::{human_size, short_id, timestamp};
use ss_core::ops::{BoxFuture, Input};
use ss_core::svc::SvcError;
use ss_core::svc::s3::{Bucket, ObjectMeta, S3Port, Version, is_current};
use unicode_width::UnicodeWidthStr;

use crate::tui::components::errorpanel::render_error;
use crate::tui::screen::{Cmd, Ctx, Loaded, Msg, Screen};
use crate::tui::screens::opsform::OpsForm;
use crate::tui::text::clean;
use crate::tui::theme;

/// The operation two marked versions are handed to.
const VERSION_DIFF: &str = "s3.version-diff";
/// The operation `R` opens for the selected version.
const RESTORE: &str = "s3.restore-version";
/// How to compare two versions, said when `d` cannot.
const HOW_TO_DIFF: &str = "mark two versions with space, then press d";
/// How times are shown in listings.
const TIME: &str = "%Y-%m-%d %H:%M";

/// What is being listed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Level {
    /// Every bucket.
    Buckets,
    /// The prefixes and objects directly under a prefix.
    Objects { bucket: String, prefix: String },
    /// The versions of one object.
    Versions { bucket: String, key: String },
}

/// One line of a listing.
enum Row {
    Bucket(Bucket),
    /// A common prefix, in full.
    Prefix(String),
    Object(ObjectMeta),
    /// The listing continues; the token fetches the next page.
    More(String),
    Version(Version),
}

impl Row {
    /// What tells this row apart from the others at its level.
    fn identity(&self) -> &str {
        match self {
            Row::Bucket(bucket) => &bucket.name,
            Row::Prefix(prefix) => prefix,
            Row::Object(object) => &object.key,
            Row::More(_) => "",
            Row::Version(version) => &version.id,
        }
    }
}

/// A listing that has been asked for.
struct Pending {
    /// What the browser will show once it arrives.
    level: Level,
    /// Whether it continues the listing already shown.
    more: bool,
}

/// The S3 browser.
pub struct S3Browse {
    s3: Option<Arc<dyn S3Port>>,
    level: Level,
    rows: Vec<Row>,
    selected: usize,
    /// The row to select again when the listing in flight arrives.
    reselect: Option<String>,
    pending: Option<Pending>,
    /// The number of the last listing asked for; other results are stale.
    request: u64,
    /// The ids of the marked versions, oldest mark first; at most two.
    marks: Vec<String>,
    /// Shown instead of a listing when there is none to show.
    error: Option<String>,
    /// Whether any listing has arrived yet.
    loaded: bool,
    /// How many rows were last drawn; a page moves by this.
    page: Cell<usize>,
}

impl S3Browse {
    /// A browser that starts on the bucket list.
    pub fn new(ctx: &Ctx) -> Self {
        let s3 = ctx.deps.s3.clone();
        let error = s3
            .is_none()
            .then(|| "S3 is not available in this session".to_owned());
        S3Browse {
            s3,
            level: Level::Buckets,
            rows: Vec::new(),
            selected: 0,
            reselect: None,
            pending: None,
            request: 0,
            marks: Vec::new(),
            error,
            loaded: false,
            page: Cell::new(1),
        }
    }

    /// Asks for the listing of `level`, continuing after `token` when given.
    fn load(&mut self, level: Level, token: Option<String>) -> Vec<Cmd> {
        let Some(s3) = self.s3.clone() else {
            return Vec::new();
        };
        self.request += 1;
        let more = token.is_some();
        let listing: BoxFuture<Loaded> = match level.clone() {
            Level::Buckets => {
                Box::pin(
                    async move { s3.list_buckets().await.map_or_else(failed, Loaded::Buckets) },
                )
            }
            Level::Objects { bucket, prefix } => Box::pin(async move {
                s3.list(&bucket, &prefix, token.as_deref())
                    .await
                    .map_or_else(failed, Loaded::Page)
            }),
            Level::Versions { bucket, key } => Box::pin(async move {
                s3.list_versions(&bucket, &key)
                    .await
                    .map_or_else(failed, Loaded::Versions)
            }),
        };
        self.pending = Some(Pending { level, more });
        vec![Cmd::Task(self.request, listing)]
    }

    fn arrived(&mut self, loaded: Loaded) -> Vec<Cmd> {
        let Some(pending) = self.pending.take() else {
            return Vec::new();
        };
        let reselect = self.reselect.take();
        let rows: Vec<Row> = match loaded {
            Loaded::Failed(e) => {
                // With a listing on screen the failure is a status line and
                // the listing stays; without one it takes the screen.
                if self.loaded {
                    return vec![Cmd::Status(format!("S3: {e}"))];
                }
                self.error = Some(e);
                return Vec::new();
            }
            Loaded::Buckets(buckets) => buckets.into_iter().map(Row::Bucket).collect(),
            Loaded::Page(page) => page
                .prefixes
                .into_iter()
                .map(Row::Prefix)
                .chain(page.objects.into_iter().map(Row::Object))
                .chain(page.next.map(Row::More))
                .collect(),
            Loaded::Versions(versions) => versions.into_iter().map(Row::Version).collect(),
            Loaded::Suggestions(_) | Loaded::Pods(_) | Loaded::NoPodsUnconfirmed(_) => {
                return Vec::new();
            }
        };
        self.loaded = true;
        self.error = None;
        if pending.more {
            // The new rows take the place of the row that asked for them.
            if matches!(self.rows.last(), Some(Row::More(_))) {
                self.rows.pop();
            }
            self.selected = self.rows.len();
            self.rows.extend(rows);
        } else {
            self.marks.retain(|id| {
                pending.level == self.level && rows.iter().any(|r| r.identity() == id)
            });
            self.level = pending.level;
            self.selected = reselect
                .and_then(|wanted| rows.iter().position(|row| row.identity() == wanted))
                .unwrap_or(0);
            self.rows = rows;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        Vec::new()
    }

    /// Opens the selected row: a bucket or prefix lists what is under it,
    /// an object lists its versions, and the last row of a cut-off listing
    /// fetches the rest.
    fn open(&mut self) -> Vec<Cmd> {
        let bucket = match &self.level {
            Level::Buckets => String::new(),
            Level::Objects { bucket, .. } | Level::Versions { bucket, .. } => bucket.clone(),
        };
        match self.rows.get(self.selected) {
            Some(Row::Bucket(chosen)) => {
                let level = Level::Objects {
                    bucket: chosen.name.clone(),
                    prefix: String::new(),
                };
                self.load(level, None)
            }
            Some(Row::Prefix(prefix)) => {
                let level = Level::Objects {
                    bucket,
                    prefix: prefix.clone(),
                };
                self.load(level, None)
            }
            Some(Row::Object(object)) => {
                let level = Level::Versions {
                    bucket,
                    key: object.key.clone(),
                };
                self.load(level, None)
            }
            Some(Row::More(token)) => {
                let token = token.clone();
                self.load(self.level.clone(), Some(token))
            }
            Some(Row::Version(_)) | None => Vec::new(),
        }
    }

    /// Goes up one level, or leaves the browser from the bucket list.
    fn up(&mut self) -> Vec<Cmd> {
        let (level, came_from) = match &self.level {
            Level::Buckets => return vec![Cmd::Pop],
            Level::Objects { bucket, prefix } if prefix.is_empty() => {
                (Level::Buckets, bucket.clone())
            }
            Level::Objects { bucket, prefix } => (
                Level::Objects {
                    bucket: bucket.clone(),
                    prefix: parent(prefix),
                },
                prefix.clone(),
            ),
            Level::Versions { bucket, key } => (
                Level::Objects {
                    bucket: bucket.clone(),
                    prefix: parent(key),
                },
                key.clone(),
            ),
        };
        self.reselect = Some(came_from);
        self.load(level, None)
    }

    fn reload(&mut self) -> Vec<Cmd> {
        self.reselect = self
            .rows
            .get(self.selected)
            .map(|row| row.identity().to_owned());
        self.load(self.level.clone(), None)
    }

    /// Marks or unmarks the selected version. A third mark replaces the
    /// oldest one.
    fn mark(&mut self) -> Vec<Cmd> {
        let Some(Row::Version(version)) = self.rows.get(self.selected) else {
            return Vec::new();
        };
        if version.is_delete {
            return status("a delete marker has no content to compare");
        }
        if let Some(position) = self.marks.iter().position(|id| *id == version.id) {
            self.marks.remove(position);
        } else {
            if self.marks.len() == 2 {
                self.marks.remove(0);
            }
            self.marks.push(version.id.clone());
        }
        Vec::new()
    }

    /// Opens the version-diff form for the two marked versions, the older
    /// one on the left.
    fn diff(&self, ctx: &Ctx) -> Vec<Cmd> {
        let Level::Versions { bucket, key } = &self.level else {
            return status(HOW_TO_DIFF);
        };
        // Versions are listed newest first.
        let marked: Vec<&Version> = self
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Version(version) if self.marks.contains(&version.id) => Some(version),
                _ => None,
            })
            .collect();
        let [newer, older] = marked.as_slice() else {
            return status(HOW_TO_DIFF);
        };
        // An object written before versioning was turned on has the id
        // `null`, which also means "the current version". Passing it for
        // anything but the latest entry would compare the wrong version.
        if let Some(unaddressable) = [newer, older]
            .into_iter()
            .find(|version| is_current(&version.id) && !version.is_latest)
        {
            return status(&format!(
                "the version with id {:?} can only be compared from the command line",
                unaddressable.id
            ));
        }
        let Some(op) = ctx.registry.get(VERSION_DIFF) else {
            return status(&format!("{VERSION_DIFF} is not available"));
        };
        let mut input = Input::default();
        input.insert("bucket", bucket.as_str());
        input.insert("key", key.as_str());
        input.insert("a", older.id.as_str());
        input.insert("b", newer.id.as_str());
        vec![Cmd::Push(Box::new(OpsForm::with_input(op, &input, ctx)))]
    }

    /// Opens the restore form for the selected version.
    fn restore(&self, ctx: &Ctx) -> Vec<Cmd> {
        let (Level::Versions { bucket, key }, Some(Row::Version(version))) =
            (&self.level, self.rows.get(self.selected))
        else {
            return status("open an object's versions and select the one to restore");
        };
        if version.is_delete {
            return status("a delete marker has no content to restore");
        }
        if version.is_latest {
            return status("this is already the current version");
        }
        // The id `null` also means "the current version", so it cannot
        // name an older one.
        if is_current(&version.id) {
            return status(&format!(
                "the version with id {:?} has no id of its own and cannot be restored",
                version.id
            ));
        }
        let Some(op) = ctx.registry.get(RESTORE) else {
            return status(&format!("{RESTORE} is not available"));
        };
        let mut input = Input::default();
        input.insert("bucket", bucket.as_str());
        input.insert("key", key.as_str());
        input.insert("version", version.id.as_str());
        vec![Cmd::Push(Box::new(OpsForm::with_input(op, &input, ctx)))]
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Vec<Cmd> {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Vec::new();
        }
        let last = self.rows.len().saturating_sub(1);
        let page = self.page.get().max(1);
        match key.code {
            KeyCode::Esc => return self.up(),
            KeyCode::Enter => return self.open(),
            KeyCode::Char('r') => return self.reload(),
            KeyCode::Char(' ') => return self.mark(),
            KeyCode::Char('d') => return self.diff(ctx),
            KeyCode::Char('R') => return self.restore(ctx),
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(last),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
            KeyCode::PageDown => self.selected = (self.selected + page).min(last),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = last,
            _ => {}
        }
        Vec::new()
    }

    /// The text of every row, in order.
    fn row_texts(&self) -> Vec<String> {
        let under = match &self.level {
            Level::Objects { prefix, .. } => prefix.as_str(),
            Level::Buckets | Level::Versions { .. } => "",
        };
        let name = |full: &str| clean(full.strip_prefix(under).unwrap_or(full));
        let name_cols = self
            .rows
            .iter()
            .map(|row| match row {
                Row::Object(object) => name(&object.key).width(),
                Row::Version(version) => short_id(&version.id).width(),
                Row::Bucket(_) | Row::Prefix(_) | Row::More(_) => 0,
            })
            .max()
            .unwrap_or(0);
        let padded = |text: String| {
            let pad = " ".repeat(name_cols.saturating_sub(text.width()));
            format!("{text}{pad}")
        };
        self.rows
            .iter()
            .map(|row| match row {
                Row::Bucket(bucket) => clean(&bucket.name),
                Row::Prefix(prefix) => name(prefix),
                Row::More(_) => "more…".to_owned(),
                Row::Object(object) => format!(
                    "{}  {:>10}  {}",
                    padded(name(&object.key)),
                    human_size(object.size),
                    timestamp(object.modified, TIME)
                ),
                Row::Version(version) => {
                    let mark = if self.marks.contains(&version.id) {
                        "[x]"
                    } else {
                        "[ ]"
                    };
                    let note = match (version.is_delete, version.is_latest) {
                        (true, true) => "delete marker · latest",
                        (true, false) => "delete marker",
                        (false, true) => "latest",
                        (false, false) => "",
                    };
                    format!(
                        "{mark} {}  {}  {:>10}  {note}",
                        padded(clean(&short_id(&version.id))),
                        timestamp(version.modified, TIME),
                        human_size(version.size)
                    )
                }
            })
            .collect()
    }
}

fn failed(e: SvcError) -> Loaded {
    Loaded::Failed(format!("{:#}", anyhow::Error::new(e)))
}

fn status(text: &str) -> Vec<Cmd> {
    vec![Cmd::Status(text.to_owned())]
}

/// The prefix one level above `path`, which is a key or a prefix ending in
/// `/`: `a/b/` and `a/b/k.json` are both under `a/` and `a/b/` in turn.
fn parent(path: &str) -> String {
    let inner = path.strip_suffix('/').unwrap_or(path);
    inner
        .rfind('/')
        .and_then(|cut| inner.get(..=cut))
        .unwrap_or_default()
        .to_owned()
}

impl Screen for S3Browse {
    fn title(&self) -> String {
        let place = match &self.level {
            Level::Buckets => "buckets".to_owned(),
            Level::Objects { bucket, prefix } => format!("s3://{bucket}/{prefix}"),
            Level::Versions { bucket, key } => format!("s3://{bucket}/{key} versions"),
        };
        let loading = if self.pending.is_some() {
            " · loading…"
        } else {
            ""
        };
        clean(&format!("S3 — {place}{loading}"))
    }

    fn hints(&self) -> String {
        match self.level {
            Level::Buckets => "↑↓ move · enter open · r reload · esc back",
            Level::Objects { .. } => "↑↓ move · enter open · r reload · esc up",
            Level::Versions { .. } => {
                "↑↓ move · space mark · d diff marked · R restore · r reload · esc up"
            }
        }
        .to_owned()
    }

    fn init(&mut self, _ctx: &Ctx) -> Vec<Cmd> {
        self.load(Level::Buckets, None)
    }

    fn update(&mut self, msg: Msg, ctx: &Ctx) -> Vec<Cmd> {
        match msg {
            Msg::Key(key) => self.on_key(key, ctx),
            Msg::Loaded(request, loaded) if request == self.request => self.arrived(loaded),
            Msg::Loaded(..)
            | Msg::Resize
            | Msg::Paste(_)
            | Msg::Progress(_)
            | Msg::Warning(_)
            | Msg::Confirm(_)
            | Msg::RunDone(_)
            | Msg::Lines(..)
            | Msg::StreamEnded(..) => Vec::new(),
        }
    }

    fn view(&self, frame: &mut Frame, area: Rect) {
        if let Some(error) = &self.error {
            render_error(frame, area, error);
            return;
        }
        if self.rows.is_empty() {
            let text = match (&self.level, self.loaded) {
                (_, false) => "loading…",
                (Level::Buckets, true) => "no buckets",
                (Level::Objects { .. }, true) => "no objects",
                (Level::Versions { .. }, true) => "no versions",
            };
            frame.render_widget(Line::styled(text, theme::dim()), area);
            return;
        }
        let visible = usize::from(area.height);
        self.page.set(visible);
        let top = (self.selected + 1).saturating_sub(visible);
        let lines: Vec<Line> = self
            .row_texts()
            .into_iter()
            .enumerate()
            .skip(top)
            .take(visible)
            .map(|(index, text)| {
                if index == self.selected {
                    Line::styled(format!("> {text}"), theme::selected())
                } else {
                    Line::from(format!("  {text}"))
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use chrono::{DateTime, TimeZone, Utc};
    use ratatui::crossterm::event::KeyCode;
    use ss_core::ops;
    use ss_core::svc::SvcError;
    use ss_core::svc::s3::{Bucket, FakeS3, Object, ObjectMeta, Page, S3Port, Version};

    use super::*;
    use crate::tui::screen::Loaded;
    use crate::tui::shell::Shell;
    use crate::tui::testing::{ch, ctx, key, press, render_screen, render_shell, settle};

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, hour, 0, 0)
            .single()
            .unwrap()
    }

    /// Two buckets; `config-dev` holds one top-level object and an `app/`
    /// prefix with a three-version object and a one-version object.
    fn seeded() -> FakeS3 {
        let fake = FakeS3::new();
        fake.create_bucket("logs-dev");
        fake.insert(
            "config-dev",
            "app/orders.json",
            "v1",
            r#"{"limit":100}"#,
            at(1),
        );
        fake.insert(
            "config-dev",
            "app/orders.json",
            "v2",
            r#"{"limit":250}"#,
            at(2),
        );
        fake.insert(
            "config-dev",
            "app/orders.json",
            "v3",
            r#"{"limit":300}"#,
            at(3),
        );
        fake.insert("config-dev", "app/users.json", "u1", "{}", at(1));
        fake.insert("config-dev", "readme.txt", "r1", "hi", at(1));
        fake
    }

    fn ctx_with(port: impl S3Port + 'static) -> Ctx {
        let mut ctx = ctx();
        ctx.deps.s3 = Some(Arc::new(port));
        ctx.registry = ops::builtin();
        ctx
    }

    fn browser(ctx: Ctx) -> Shell {
        let screen = S3Browse::new(&ctx);
        Shell::new(ctx, Box::new(screen))
    }

    fn enter() -> ratatui::crossterm::event::KeyEvent {
        key(KeyCode::Enter)
    }

    fn down() -> ratatui::crossterm::event::KeyEvent {
        key(KeyCode::Down)
    }

    fn esc() -> ratatui::crossterm::event::KeyEvent {
        key(KeyCode::Esc)
    }

    /// A browser showing the versions of `app/orders.json`.
    async fn on_versions(ctx: Ctx) -> Shell {
        let mut shell = browser(ctx);
        settle(&mut shell, "logs-dev").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "readme.txt").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "users.json").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "v3").await;
        shell
    }

    /// The text of the row the selection is on.
    fn selected(shell: &Shell) -> String {
        render_shell(shell, 80, 24)
            .lines()
            .find_map(|line| line.strip_prefix("> "))
            .unwrap_or_default()
            .to_owned()
    }

    /// The value shown for the form field labelled `label`.
    fn field(shell: &Shell, label: &str) -> String {
        render_shell(shell, 80, 24)
            .lines()
            .find(|line| line.contains(label))
            .and_then(|line| line.split_whitespace().last())
            .unwrap_or_default()
            .to_owned()
    }

    /// A port that fails every call while `failing` is set.
    struct Flaky {
        inner: FakeS3,
        failing: Arc<AtomicBool>,
    }

    impl Flaky {
        fn check(&self) -> Result<(), SvcError> {
            if self.failing.load(Ordering::SeqCst) {
                Err(SvcError::Other(anyhow::anyhow!("AccessDenied")))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl S3Port for Flaky {
        async fn list_buckets(&self) -> Result<Vec<Bucket>, SvcError> {
            self.check()?;
            self.inner.list_buckets().await
        }

        async fn list(
            &self,
            bucket: &str,
            prefix: &str,
            token: Option<&str>,
        ) -> Result<Page, SvcError> {
            self.check()?;
            self.inner.list(bucket, prefix, token).await
        }

        async fn list_versions(&self, bucket: &str, key: &str) -> Result<Vec<Version>, SvcError> {
            self.check()?;
            self.inner.list_versions(bucket, key).await
        }

        async fn get_version(
            &self,
            bucket: &str,
            key: &str,
            version_id: &str,
        ) -> Result<Object, SvcError> {
            self.check()?;
            self.inner.get_version(bucket, key, version_id).await
        }

        async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, SvcError> {
            self.check()?;
            self.inner.head(bucket, key).await
        }

        async fn copy_version(
            &self,
            bucket: &str,
            key: &str,
            version_id: &str,
        ) -> Result<String, SvcError> {
            self.check()?;
            self.inner.copy_version(bucket, key, version_id).await
        }
    }

    #[tokio::test]
    async fn the_browser_opens_on_the_bucket_list() {
        let mut shell = browser(ctx_with(seeded()));

        let shown = settle(&mut shell, "logs-dev").await;

        assert!(shown.contains("S3 — buckets"), "{shown}");
        assert_eq!(selected(&shell), "config-dev");
    }

    #[tokio::test]
    async fn enter_descends_bucket_then_prefix_and_esc_goes_back_up() {
        let mut shell = browser(ctx_with(seeded()));
        settle(&mut shell, "logs-dev").await;

        press(&mut shell, &[enter()]);
        let shown = settle(&mut shell, "readme.txt").await;
        assert!(shown.contains("S3 — s3://config-dev/"), "{shown}");
        assert_eq!(selected(&shell), "app/");

        press(&mut shell, &[enter()]);
        let shown = settle(&mut shell, "users.json").await;
        assert!(shown.contains("S3 — s3://config-dev/app/"), "{shown}");
        assert!(selected(&shell).starts_with("orders.json"), "{shown}");

        press(&mut shell, &[esc()]);
        settle(&mut shell, "readme.txt").await;
        press(&mut shell, &[esc()]);
        let shown = settle(&mut shell, "logs-dev").await;
        assert!(shown.contains("S3 — buckets"), "{shown}");
        assert_eq!(shell.depth(), 1);
    }

    #[tokio::test]
    async fn esc_on_the_bucket_list_leaves_the_browser() {
        let ctx = ctx_with(seeded());
        let mut screen = S3Browse::new(&ctx);
        screen.init(&ctx);

        let cmds = screen.update(Msg::Key(esc()), &ctx);

        assert!(matches!(cmds.as_slice(), [Cmd::Pop]));
    }

    #[tokio::test]
    async fn a_truncated_page_loads_more_from_the_trailing_row() {
        let fake = FakeS3::new();
        for name in ["a.json", "b.json", "c.json"] {
            fake.insert("config-dev", name, "v1", "{}", at(1));
        }
        let mut shell = browser(ctx_with(fake.paged(2)));
        settle(&mut shell, "config-dev").await;
        press(&mut shell, &[enter()]);
        let shown = settle(&mut shell, "more…").await;
        assert!(
            shown.contains("b.json") && !shown.contains("c.json"),
            "{shown}"
        );

        press(&mut shell, &[down(), down(), enter()]);
        let shown = settle(&mut shell, "c.json").await;

        assert!(
            shown.contains("a.json") && shown.contains("b.json"),
            "{shown}"
        );
        assert!(!shown.contains("more…"), "{shown}");
        assert!(selected(&shell).starts_with("c.json"), "{shown}");
    }

    #[tokio::test]
    async fn enter_on_an_object_lists_its_versions_newest_first() {
        let shell = on_versions(ctx_with(seeded())).await;

        let shown = render_shell(&shell, 80, 24);
        assert!(
            shown.contains("S3 — s3://config-dev/app/orders.json versions"),
            "{shown}"
        );
        let ids: Vec<&str> = shown
            .lines()
            .filter_map(|line| line.get(2..).and_then(|rest| rest.strip_prefix("[ ] ")))
            .filter_map(|rest| rest.split_whitespace().next())
            .collect();
        assert_eq!(ids, ["v3", "v2", "v1"]);
        assert!(selected(&shell).contains("latest"), "{shown}");
    }

    #[tokio::test]
    async fn space_marks_two_versions_and_a_third_mark_replaces_the_older_mark() {
        let mut shell = on_versions(ctx_with(seeded())).await;

        press(&mut shell, &[ch(' '), down(), ch(' ')]);
        let shown = render_shell(&shell, 80, 24);
        assert!(
            shown.contains("[x] v3") && shown.contains("[x] v2"),
            "{shown}"
        );

        press(&mut shell, &[down(), ch(' ')]);
        let shown = render_shell(&shell, 80, 24);
        assert!(shown.contains("[ ] v3"), "the first mark gave way: {shown}");
        assert!(
            shown.contains("[x] v2") && shown.contains("[x] v1"),
            "{shown}"
        );

        press(&mut shell, &[ch(' ')]);
        assert!(
            render_shell(&shell, 80, 24).contains("[ ] v1"),
            "space unmarks"
        );
    }

    #[tokio::test]
    async fn delete_markers_are_shown_and_cannot_be_marked() {
        // Deleted, then written again: the marker sits in the history of an
        // object that is still listed.
        let fake = seeded();
        fake.delete("config-dev", "app/orders.json");
        fake.put("config-dev", "app/orders.json", "{}");
        let mut shell = on_versions(ctx_with(fake)).await;
        press(&mut shell, &[down()]);
        assert!(
            selected(&shell).contains("delete marker"),
            "{}",
            selected(&shell)
        );

        press(&mut shell, &[ch(' ')]);

        let shown = render_shell(&shell, 80, 24);
        assert!(!shown.contains("[x]"), "{shown}");
        assert!(
            shown.contains("a delete marker has no content to compare"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn d_without_two_marks_says_mark_two_versions() {
        let mut shell = on_versions(ctx_with(seeded())).await;

        press(&mut shell, &[ch(' '), ch('d')]);

        assert!(
            render_shell(&shell, 80, 24).contains("mark two versions with space, then press d")
        );
        assert_eq!(shell.depth(), 1);
    }

    #[tokio::test]
    async fn d_opens_the_version_diff_form_with_the_older_mark_as_a() {
        // Marked newest first.
        let mut shell = on_versions(ctx_with(seeded())).await;
        press(&mut shell, &[down(), ch(' '), down(), ch(' '), ch('d')]);
        assert_eq!(shell.depth(), 2);
        assert_eq!(field(&shell, "Bucket"), "config-dev");
        assert_eq!(field(&shell, "Key"), "app/orders.json");
        assert_eq!(field(&shell, "Version A"), "v1");
        assert_eq!(field(&shell, "Version B"), "v2");

        // Marked oldest first.
        let mut shell = on_versions(ctx_with(seeded())).await;
        press(
            &mut shell,
            &[down(), down(), ch(' '), key(KeyCode::Up), ch(' '), ch('d')],
        );
        assert_eq!(field(&shell, "Version A"), "v1");
        assert_eq!(field(&shell, "Version B"), "v2");
    }

    #[tokio::test]
    async fn the_latest_version_is_compared_by_its_own_id() {
        let mut shell = on_versions(ctx_with(seeded())).await;

        press(&mut shell, &[ch(' '), down(), down(), ch(' '), ch('d')]);

        // Not as "current": a version uploaded after the marking would
        // otherwise be compared instead of the one that was marked.
        assert_eq!(field(&shell, "Version A"), "v1");
        assert_eq!(field(&shell, "Version B"), "v3");
    }

    #[tokio::test]
    async fn d_reports_when_version_diff_is_not_registered() {
        let mut ctx = ctx_with(seeded());
        ctx.registry = ops::Registry::new();
        let mut shell = on_versions(ctx).await;

        press(&mut shell, &[ch(' '), down(), ch(' '), ch('d')]);

        assert!(render_shell(&shell, 80, 24).contains("s3.version-diff is not available"));
        assert_eq!(shell.depth(), 1);
    }

    #[tokio::test]
    async fn r_reloads_the_current_level_and_keeps_the_selection_when_it_still_exists() {
        let fake = seeded();
        let mut shell = browser(ctx_with(fake.clone()));
        settle(&mut shell, "logs-dev").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "readme.txt").await;
        press(&mut shell, &[down()]);
        assert!(selected(&shell).starts_with("readme.txt"));

        fake.insert("config-dev", "about.txt", "a1", "new", at(4));
        press(&mut shell, &[ch('r')]);
        let shown = settle(&mut shell, "about.txt").await;

        assert!(selected(&shell).starts_with("readme.txt"), "{shown}");
    }

    #[tokio::test]
    async fn a_missing_s3_port_shows_the_error_panel_and_esc_still_leaves() {
        let ctx = ctx();
        let mut screen = S3Browse::new(&ctx);

        assert!(screen.init(&ctx).is_empty());

        let shown = render_screen(&screen, 60, 6);
        assert!(shown.contains("error"), "{shown}");
        assert!(shown.contains("S3 is not available"), "{shown}");
        assert!(matches!(
            screen.update(Msg::Key(esc()), &ctx).as_slice(),
            [Cmd::Pop]
        ));
    }

    #[tokio::test]
    async fn a_first_listing_that_fails_shows_the_error_and_r_tries_again() {
        let failing = Arc::new(AtomicBool::new(true));
        let port = Flaky {
            inner: seeded(),
            failing: Arc::clone(&failing),
        };
        let mut shell = browser(ctx_with(port));
        let shown = settle(&mut shell, "AccessDenied").await;
        assert!(shown.contains("error"), "{shown}");

        failing.store(false, Ordering::SeqCst);
        press(&mut shell, &[ch('r')]);

        let shown = settle(&mut shell, "logs-dev").await;
        assert!(!shown.contains("AccessDenied"), "{shown}");
    }

    #[tokio::test]
    async fn a_failed_listing_shows_the_error_and_keeps_the_previous_level() {
        let failing = Arc::new(AtomicBool::new(false));
        let port = Flaky {
            inner: seeded(),
            failing: Arc::clone(&failing),
        };
        let mut shell = browser(ctx_with(port));
        settle(&mut shell, "logs-dev").await;

        failing.store(true, Ordering::SeqCst);
        press(&mut shell, &[enter()]);
        let shown = settle(&mut shell, "AccessDenied").await;

        assert!(shown.contains("S3 — buckets"), "{shown}");
        assert!(shown.contains("logs-dev"), "{shown}");
        assert_eq!(selected(&shell), "config-dev");
    }

    #[tokio::test]
    async fn a_stale_listing_response_is_ignored() {
        let ctx = ctx_with(seeded());
        let mut screen = S3Browse::new(&ctx);
        let first = match screen.init(&ctx).pop() {
            Some(Cmd::Task(id, _)) => id,
            _ => panic!("the browser lists the buckets first"),
        };
        let second = match screen.update(Msg::Key(ch('r')), &ctx).pop() {
            Some(Cmd::Task(id, _)) => id,
            _ => panic!("r lists again"),
        };
        let bucket = |name: &str| Bucket {
            name: name.to_owned(),
            created: None,
        };

        screen.update(
            Msg::Loaded(first, Loaded::Buckets(vec![bucket("stale")])),
            &ctx,
        );
        assert!(!render_screen(&screen, 60, 6).contains("stale"));

        screen.update(
            Msg::Loaded(second, Loaded::Buckets(vec![bucket("fresh")])),
            &ctx,
        );
        assert!(render_screen(&screen, 60, 6).contains("fresh"));
    }

    #[tokio::test]
    async fn an_empty_bucket_says_no_objects() {
        let mut shell = browser(ctx_with(seeded()));
        settle(&mut shell, "logs-dev").await;

        press(&mut shell, &[down(), enter()]);

        let shown = settle(&mut shell, "no objects").await;
        assert!(shown.contains("S3 — s3://logs-dev/"), "{shown}");
    }

    #[tokio::test]
    async fn an_account_without_buckets_says_no_buckets() {
        let mut shell = browser(ctx_with(FakeS3::new()));

        settle(&mut shell, "no buckets").await;
    }

    #[tokio::test]
    async fn keys_with_control_characters_are_listed_safely() {
        let fake = FakeS3::new();
        fake.insert("config-dev", "evil\x1b[2Jname.json", "v1", "{}", at(1));
        let mut shell = browser(ctx_with(fake));
        settle(&mut shell, "config-dev").await;

        press(&mut shell, &[enter()]);

        settle(&mut shell, "evil[2Jname.json").await;
    }

    #[tokio::test]
    async fn the_selection_stays_in_view_in_a_long_listing() {
        let fake = FakeS3::new();
        for i in 0..60 {
            fake.insert("config-dev", &format!("k{i:02}.json"), "v1", "{}", at(1));
        }
        let mut shell = browser(ctx_with(fake));
        settle(&mut shell, "config-dev").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "k00.json").await;

        press(&mut shell, &[down(); 45]);

        assert!(
            selected(&shell).starts_with("k45.json"),
            "{}",
            render_shell(&shell, 80, 24)
        );

        press(&mut shell, &[ch('g')]);
        assert!(selected(&shell).starts_with("k00.json"));
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        let ctx = ctx();
        let screen = S3Browse::new(&ctx);

        render_screen(&screen, 1, 1);
        render_screen(&screen, 0, 0);
    }

    #[tokio::test]
    async fn a_version_without_an_id_cannot_be_compared_unless_it_is_the_latest() {
        // An object written before versioning was turned on keeps the id
        // "null", which the port reads as "the current version".
        let fake = FakeS3::new();
        fake.create_bucket("logs-dev");
        fake.insert(
            "config-dev",
            "app/orders.json",
            "null",
            r#"{"limit":1}"#,
            at(1),
        );
        fake.insert(
            "config-dev",
            "app/orders.json",
            "v2",
            r#"{"limit":2}"#,
            at(2),
        );
        fake.insert("config-dev", "app/users.json", "u1", "{}", at(1));
        fake.insert("config-dev", "readme.txt", "r1", "hi", at(1));
        let mut shell = browser(ctx_with(fake));
        settle(&mut shell, "logs-dev").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "readme.txt").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "users.json").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "v2").await;

        press(&mut shell, &[ch(' '), down(), ch(' '), ch('d')]);

        let shown = render_shell(&shell, 80, 24);
        assert_eq!(shell.depth(), 1, "no form was opened: {shown}");
        assert!(
            shown.contains(
                "the version with id \"null\" can only be compared from the command line"
            ),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn marks_survive_a_reload_while_their_versions_still_exist() {
        let mut shell = on_versions(ctx_with(seeded())).await;
        press(&mut shell, &[ch(' '), down(), ch(' ')]);

        press(&mut shell, &[ch('r')]);
        shell.pump().await;

        let shown = render_shell(&shell, 80, 24);
        assert!(
            shown.contains("[x] v3") && shown.contains("[x] v2"),
            "{shown}"
        );
    }

    fn shift(c: char) -> ratatui::crossterm::event::KeyEvent {
        ratatui::crossterm::event::KeyEvent::new(
            KeyCode::Char(c),
            ratatui::crossterm::event::KeyModifiers::SHIFT,
        )
    }

    #[tokio::test]
    async fn shift_r_opens_the_restore_form_for_the_selected_version() {
        let mut shell = on_versions(ctx_with(seeded())).await;

        press(&mut shell, &[down(), down(), shift('R')]);

        assert_eq!(shell.depth(), 2);
        assert!(render_shell(&shell, 80, 24).contains("Restore object version"));
        assert_eq!(field(&shell, "Bucket"), "config-dev");
        assert_eq!(field(&shell, "Key"), "app/orders.json");
        assert_eq!(field(&shell, "Version"), "v1");
    }

    #[tokio::test]
    async fn shift_r_on_what_cannot_be_restored_says_why() {
        // The latest version.
        let mut shell = on_versions(ctx_with(seeded())).await;
        press(&mut shell, &[shift('R')]);
        assert!(
            render_shell(&shell, 80, 24).contains("this is already the current version"),
            "{}",
            render_shell(&shell, 80, 24)
        );
        assert_eq!(shell.depth(), 1);

        // A delete marker.
        let fake = seeded();
        fake.delete("config-dev", "app/orders.json");
        fake.put("config-dev", "app/orders.json", "{}");
        let mut shell = on_versions(ctx_with(fake)).await;
        press(&mut shell, &[down(), shift('R')]);
        assert!(
            render_shell(&shell, 80, 24).contains("a delete marker has no content to restore"),
            "{}",
            render_shell(&shell, 80, 24)
        );
        assert_eq!(shell.depth(), 1);
    }

    #[tokio::test]
    async fn shift_r_on_a_version_without_an_id_says_it_cannot_be_named() {
        let fake = FakeS3::new();
        fake.insert("config-dev", "app/orders.json", "null", "{}", at(1));
        fake.insert("config-dev", "app/orders.json", "v2", "{}", at(2));
        let mut shell = browser(ctx_with(fake));
        settle(&mut shell, "config-dev").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "app/").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "orders.json").await;
        press(&mut shell, &[enter()]);
        settle(&mut shell, "v2").await;

        press(&mut shell, &[down(), shift('R')]);

        assert!(
            render_shell(&shell, 80, 24).contains("has no id of its own and cannot be restored"),
            "{}",
            render_shell(&shell, 80, 24)
        );
        assert_eq!(shell.depth(), 1);
    }

    #[tokio::test]
    async fn shift_r_outside_a_version_list_says_where_to_go() {
        let mut shell = browser(ctx_with(seeded()));
        settle(&mut shell, "logs-dev").await;

        press(&mut shell, &[shift('R')]);

        assert!(
            render_shell(&shell, 80, 24)
                .contains("open an object's versions and select the one to restore")
        );
        assert_eq!(shell.depth(), 1);
    }

    #[tokio::test]
    async fn shift_r_reports_when_restore_is_not_registered() {
        let mut ctx = ctx_with(seeded());
        ctx.registry = ops::Registry::new();
        let mut shell = on_versions(ctx).await;

        press(&mut shell, &[down(), shift('R')]);

        assert!(render_shell(&shell, 80, 24).contains("s3.restore-version is not available"));
        assert_eq!(shell.depth(), 1);
    }

    #[tokio::test]
    async fn the_version_list_says_how_to_restore() {
        let shell = on_versions(ctx_with(seeded())).await;

        assert!(render_shell(&shell, 100, 24).contains("R restore"));
    }
}
