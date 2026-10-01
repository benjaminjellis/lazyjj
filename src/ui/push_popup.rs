use anyhow::Result;
use ratatui::{
    crossterm::event::{Event, KeyCode, KeyEventKind},
    prelude::*,
    widgets::*,
};

use super::{Component, ComponentAction, loader_popup::LoaderPopup, utils::centered_rect};
use crate::{ComponentInputResult, commander::Commander};

/// A successful jj push can still refuse to create untracked remote bookmarks.
/// Offer a narrowly scoped retry, retaining the original diagnostic in the popup.
pub struct PushPopup {
    output: String,
    bookmarks: Vec<(String, String)>,
    scroll: u16,
}

impl PushPopup {
    pub fn from_output(output: String) -> Option<Self> {
        let mut bookmarks = Vec::new();
        for line in output.lines() {
            if let Some(reference) =
                line.strip_prefix("Warning: Refusing to create new remote bookmark ")
                && let Some((name, remote)) = reference.trim().rsplit_once('@')
                && !name.is_empty()
                && !remote.is_empty()
                && !reference.chars().any(char::is_whitespace)
            {
                let bookmark = (name.to_owned(), remote.to_owned());
                if !bookmarks.contains(&bookmark) {
                    bookmarks.push(bookmark);
                }
            }
        }
        if bookmarks.is_empty() {
            return None;
        }
        Some(Self {
            output,
            bookmarks,
            scroll: 0,
        })
    }
}

impl Component for PushPopup {
    fn draw(&mut self, frame: &mut Frame<'_>, area: Rect) -> Result<()> {
        let area = centered_rect(area, 85, 75);
        let block = Block::bordered()
            .title(" Push warning ")
            .border_type(BorderType::Rounded);
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        let text = self.output.trim_end();
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new("Create remote - y/n").alignment(Alignment::Center),
            chunks[1],
        );
        Ok(())
    }

    fn input(&mut self, commander: &mut Commander, event: Event) -> Result<ComponentInputResult> {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Release {
                return Ok(ComponentInputResult::Handled);
            }
            match key.code {
                KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
                KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::Esc | KeyCode::Char('n' | 'q') => {
                    return Ok(ComponentInputResult::HandledAction(
                        ComponentAction::SetPopup(None),
                    ));
                }
                KeyCode::Char('y') => {
                    let commander = Commander::new(&commander.env);
                    let bookmarks = self.bookmarks.clone();
                    return Ok(ComponentInputResult::HandledAction(
                        ComponentAction::SetPopup(Some(Box::new(LoaderPopup::new(
                            "Creating remote bookmarks and pushing".to_owned(),
                            move || commander.git_push_new_bookmarks(&bookmarks),
                        )))),
                    ));
                }
                _ => {}
            }
        }
        Ok(ComponentInputResult::Handled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commander::tests::TestRepo;
    use ratatui::crossterm::event::KeyEvent;
    use std::process::Command;
    use tempdir::TempDir;

    #[test]
    fn refusal_parser_is_scoped_to_new_remote_bookmarks() {
        let output = "Warning: Refusing to create new remote bookmark feature@origin\nHint: track it\nWarning: Refusing to create new remote bookmark feature@origin\nWarning: Refusing to create new remote bookmark other@fork\n";
        let popup = PushPopup::from_output(output.into()).unwrap();
        assert_eq!(
            popup.bookmarks,
            vec![
                ("feature".into(), "origin".into()),
                ("other".into(), "fork".into())
            ]
        );
        assert!(PushPopup::from_output("Error: authentication failed".into()).is_none());
        assert!(PushPopup::from_output("Nothing changed.".into()).is_none());
        assert!(
            PushPopup::from_output(
                "Warning: Refusing to create new remote bookmark bad@origin extra".into()
            )
            .is_none()
        );
    }

    #[test]
    fn no_cancels_without_running_a_command() -> Result<()> {
        let mut repo = TestRepo::new()?;
        let mut popup = PushPopup::from_output(
            "Warning: Refusing to create new remote bookmark feature@origin".into(),
        )
        .unwrap();
        let count = repo.commander.command_history.lock().unwrap().len();
        let action = popup.input(
            &mut repo.commander,
            Event::Key(KeyEvent::new(
                KeyCode::Char('n'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
        )?;
        assert!(matches!(
            action,
            ComponentInputResult::HandledAction(ComponentAction::SetPopup(None))
        ));
        assert_eq!(repo.commander.command_history.lock().unwrap().len(), count);
        Ok(())
    }

    #[test]
    fn push_warning_and_confirmed_retry_against_local_remote() -> Result<()> {
        let repo = TestRepo::new()?;
        let remote = TempDir::new("lazyjj-push-remote")?;
        assert!(
            Command::new("git")
                .args(["init", "--bare"])
                .arg(remote.path())
                .output()?
                .status
                .success()
        );
        repo.commander.execute_void_jj_command([
            "git",
            "remote",
            "add",
            "origin",
            remote.path().to_str().unwrap(),
        ])?;
        repo.commander.run_describe("@", "Push test")?;
        repo.commander.create_bookmark("feature")?;
        repo.commander.create_bookmark("unrelated")?;
        let head = repo.commander.get_current_head()?;
        let output = repo.commander.git_push(false, false, &head.commit_id)?;
        assert!(
            output.contains("Refusing to create new remote bookmark feature@origin"),
            "{output}"
        );
        let popup = PushPopup::from_output(output).unwrap();
        assert!(
            popup
                .bookmarks
                .contains(&("feature".into(), "origin".into()))
        );
        // Accepting one explicit target must not publish the other bookmark.
        let output = repo
            .commander
            .git_push_new_bookmarks(&[("feature".into(), "origin".into())])?;
        assert!(!output.is_empty());
        let refs = Command::new("git")
            .arg("--git-dir")
            .arg(remote.path())
            .args(["for-each-ref", "--format=%(refname)"])
            .output()?;
        let refs = String::from_utf8(refs.stdout)?;
        assert!(refs.contains("refs/heads/feature"));
        assert!(!refs.contains("unrelated"));
        // Parsed queries must still return stdout alone.
        let commit = repo.commander.execute_jj_command(
            ["log", "--no-graph", "-r", "feature", "-T", "commit_id"],
            false,
            true,
        )?;
        assert_eq!(commit.trim(), head.commit_id.as_str());
        Ok(())
    }
}
