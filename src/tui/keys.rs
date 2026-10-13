use crate::requests::RequestKind;
use crate::state::PrKey;
use crate::tui::model::{kind_label, Row, RowId, UiState, View};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Esc,
    CtrlC,
    Char(char),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Focus(PrKey),
    /// Linha e a URL vista na tela (copiada na hora); o worker relê a URL do estado.
    OpenUrl(RowId, String),
    ShowLog(PrKey),
    Request(RequestKind, Option<PrKey>),
    Quit,
}

fn eligible(row: &Row, kind: RequestKind) -> bool {
    match kind {
        RequestKind::Retry => row.eligible.retry,
        RequestKind::Adopt => row.eligible.adopt,
        RequestKind::Release => row.eligible.release,
        RequestKind::Sync => true,
    }
}

fn not_eligible_msg(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Retry => "tentar de novo só vale para setup que falhou ou está bloqueado sem worktree",
        RequestKind::Adopt => "só dá para adotar um worktree gerenciado pelo daemon",
        RequestKind::Release => "só dá para liberar um worktree adotado",
        RequestKind::Sync => "",
    }
}

pub fn confirm_text(kind: RequestKind, key: &PrKey) -> String {
    match kind {
        RequestKind::Adopt => format!("adotar {key}? protege da limpeza automática e cancela o setup em andamento (y/n)"),
        RequestKind::Release => {
            format!("liberar {key}? remove o worktree; se o PR ainda estiver pendente, ele só volta com um novo pedido de review (y/n)")
        }
        other => format!("{} {key}? (y/n)", kind_label(other)),
    }
}

pub fn handle(key: Key, ui: &UiState, view: &View) -> (UiState, Option<Action>) {
    let mut next = ui.clone();

    if let Some((kind, target)) = ui.confirm.clone() {
        return match key {
            Key::Char('y') | Key::Char('Y') => {
                next.confirm = None;
                match view.row(&RowId::Review(target.clone())) {
                    Some(row) if eligible(row, kind) => (next, Some(Action::Request(kind, Some(target)))),
                    _ => {
                        next.message = Some(format!("{target} mudou de estado; pedido cancelado"));
                        (next, None)
                    }
                }
            }
            Key::Char('n') | Key::Char('N') | Key::Esc => {
                next.confirm = None;
                next.message = Some("cancelado".into());
                (next, None)
            }
            Key::CtrlC => (next, Some(Action::Quit)),
            _ => (ui.clone(), None),
        };
    }

    next.message = None;
    let selected = ui.selected_row(view);
    let index = selected.and_then(|r| view.rows.iter().position(|x| x.id == r.id));
    match key {
        Key::Up | Key::Char('k') => {
            if let Some(i) = index {
                next.selected = Some(view.rows[i.saturating_sub(1)].id.clone());
            }
            (next, None)
        }
        Key::Down | Key::Char('j') => {
            if let Some(i) = index {
                next.selected = Some(view.rows[(i + 1).min(view.rows.len() - 1)].id.clone());
            }
            (next, None)
        }
        Key::Char('q') | Key::Esc | Key::CtrlC => (next, Some(Action::Quit)),
        Key::Char('s') => (next, Some(Action::Request(RequestKind::Sync, None))),
        _ => {
            let Some(row) = selected else { return (next, None) };
            let Some(key_pr) = row.id.review() else {
                // meus PRs: só abrir no navegador
                match key {
                    Key::Enter | Key::Char('o') => return (next, Some(Action::OpenUrl(row.id.clone(), row.url.clone()))),
                    Key::Char('l' | 'R' | 'a' | 'r') => next.message = Some("só para PRs de review".into()),
                    _ => {}
                }
                return (next, None);
            };
            match key {
                Key::Enter if row.eligible.focus => (next, Some(Action::Focus(key_pr.clone()))),
                Key::Enter => {
                    next.message = Some("este PR está sendo criado ou removido".into());
                    (next, None)
                }
                Key::Char('o') => (next, Some(Action::OpenUrl(row.id.clone(), row.url.clone()))),
                Key::Char('l') if row.eligible.log => (next, Some(Action::ShowLog(key_pr.clone()))),
                Key::Char('R') if row.eligible.retry => (next, Some(Action::Request(RequestKind::Retry, Some(key_pr.clone())))),
                Key::Char('a') | Key::Char('r') => {
                    let kind = if key == Key::Char('a') { RequestKind::Adopt } else { RequestKind::Release };
                    if eligible(row, kind) {
                        // a confirmação é desenhada a partir de `confirm` (confirm_text), não de `message`
                        next.confirm = Some((kind, key_pr.clone()));
                    } else {
                        next.message = Some(not_eligible_msg(kind).into());
                    }
                    (next, None)
                }
                Key::Char('R') => {
                    next.message = Some(not_eligible_msg(RequestKind::Retry).into());
                    (next, None)
                }
                _ => (next, None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::UiFacts;
    use crate::state::{Ownership, Phase, PrRecord, RepoStatus, State};
    use crate::tui::model::{View, ViewConfig};
    use chrono::Utc;

    fn view(recs: Vec<PrRecord>) -> View {
        let mut s = State::default();
        s.repos.insert("o/r".into(), RepoStatus { ok: true, last_sync: Some(Utc::now()), ..Default::default() });
        for r in recs {
            s.insert(r);
        }
        View::build(Some(&s), &UiState::default(), &ViewConfig { repos: vec!["o/r".into()], poll_interval_secs: 60, remove_grace: chrono::Duration::minutes(15) }, &UiFacts::default(), Utc::now())
    }

    fn view_with_mine(recs: Vec<PrRecord>, mine: &[u64]) -> View {
        let mut s = State::default();
        s.repos.insert("o/r".into(), RepoStatus { ok: true, last_sync: Some(Utc::now()), ..Default::default() });
        for r in recs {
            s.insert(r);
        }
        let prs = mine
            .iter()
            .map(|n| crate::state::MyPr {
                number: *n,
                title: format!("Meu {n}"),
                url: format!("https://github.com/o/r/pull/{n}"),
                draft: false,
                created_at: Utc::now(),
                head_sha: "m".into(),
                ci: None,
            })
            .collect();
        s.my_prs.repos.insert("o/r".into(), crate::state::MyPrsRepo { prs, opened_24h: 1, as_of: Utc::now() });
        View::build(Some(&s), &UiState::default(), &ViewConfig { repos: vec!["o/r".into()], poll_interval_secs: 60, remove_grace: chrono::Duration::minutes(15) }, &UiFacts::default(), Utc::now())
    }

    fn mine_at(n: u64) -> UiState {
        UiState { selected: Some(RowId::Mine(PrKey::new("o/r", n))), ..Default::default() }
    }

    #[test]
    fn enter_and_o_on_mine_open_url() {
        let v = view_with_mine(vec![PrRecord::fixture("o/r", 9, "x", "s")], &[9]);
        let want = Action::OpenUrl(RowId::Mine(PrKey::new("o/r", 9)), "https://github.com/o/r/pull/9".into());
        assert_eq!(handle(Key::Enter, &mine_at(9), &v).1, Some(want.clone()));
        assert_eq!(handle(Key::Char('o'), &mine_at(9), &v).1, Some(want));
    }

    #[test]
    fn log_retry_adopt_release_on_mine_do_nothing() {
        let v = view_with_mine(vec![], &[9]);
        for k in ['l', 'R', 'a', 'r'] {
            let (ui, a) = handle(Key::Char(k), &mine_at(9), &v);
            assert!(a.is_none(), "{k}");
            assert!(ui.confirm.is_none(), "{k}");
            assert!(ui.message.as_deref().unwrap_or("").contains("só para PRs de review"), "{k}: {:?}", ui.message);
        }
    }

    #[test]
    fn navigation_crosses_into_mine() {
        let v = view_with_mine(vec![PrRecord::fixture("o/r", 1, "x", "s")], &[9]);
        let (ui, _) = handle(Key::Down, &UiState::default(), &v);
        assert_eq!(ui.selected, Some(RowId::Mine(PrKey::new("o/r", 9))));
    }

    #[test]
    fn open_url_on_review_carries_review_id() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s")]);
        assert_eq!(
            handle(Key::Char('o'), &UiState::default(), &v).1,
            Some(Action::OpenUrl(RowId::Review(PrKey::new("o/r", 1)), "https://github.com/o/r/pull/1".into()))
        );
    }

    fn adopted(n: u64) -> PrRecord {
        let mut r = PrRecord::fixture("o/r", n, "x", "s");
        r.ownership = Ownership::Adopted { reason: "x".into(), at: Utc::now() };
        r
    }

    #[test]
    fn navigation_moves_by_key() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s"), PrRecord::fixture("o/r", 2, "x", "s")]);
        let (ui, a) = handle(Key::Down, &UiState::default(), &v);
        assert!(a.is_none());
        assert_eq!(ui.selected, Some(RowId::Review(PrKey::new("o/r", 2))));
        let (ui, _) = handle(Key::Char('k'), &ui, &v);
        assert_eq!(ui.selected, Some(RowId::Review(PrKey::new("o/r", 1))));
    }

    #[test]
    fn release_confirmation_is_bound_to_the_chosen_pr() {
        let v = view(vec![adopted(5), adopted(6)]);
        let ui = UiState { selected: Some(RowId::Review(PrKey::new("o/r", 5))), ..Default::default() };
        let (ui, a) = handle(Key::Char('r'), &ui, &v);
        assert!(a.is_none());
        assert_eq!(ui.confirm, Some((RequestKind::Release, PrKey::new("o/r", 5))));
        // refresh: a seleção mudou para 6, mas o alvo continua 5
        let ui = UiState { selected: Some(RowId::Review(PrKey::new("o/r", 6))), ..ui };
        let (ui, a) = handle(Key::Char('y'), &ui, &v);
        assert!(matches!(a, Some(Action::Request(RequestKind::Release, Some(ref k))) if k.number == 5));
        assert!(ui.confirm.is_none());
    }

    #[test]
    fn confirmation_cancels_if_target_vanished_or_ineligible() {
        let v = view(vec![adopted(5)]);
        let ui = UiState { confirm: Some((RequestKind::Release, PrKey::new("o/r", 5))), ..Default::default() };
        let gone = view(vec![]);
        let (ui2, a) = handle(Key::Char('y'), &ui, &gone);
        assert!(a.is_none() && ui2.confirm.is_none() && ui2.message.unwrap().contains("cancelado"));
        let now_managed = view(vec![PrRecord::fixture("o/r", 5, "x", "s")]);
        let (_, a) = handle(Key::Char('y'), &ui, &now_managed);
        assert!(a.is_none());
        let (ui3, a) = handle(Key::Esc, &ui, &v);
        assert!(a.is_none() && ui3.confirm.is_none());
        let (ui4, a) = handle(Key::Char('s'), &ui, &v);
        assert!(a.is_none() && ui4.confirm.is_some(), "outras teclas são ignoradas na confirmação");
    }

    #[test]
    fn eligibility_is_enforced() {
        let mut failed = PrRecord::fixture("o/r", 2, "x", "s");
        failed.phase = Phase::Failed { step: "a".into(), reason: "b".into() };
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s"), failed, adopted(5)]);
        let at = |n| UiState { selected: Some(RowId::Review(PrKey::new("o/r", n))), ..Default::default() };
        let (ui, a) = handle(Key::Char('R'), &at(1), &v);
        assert!(a.is_none() && ui.message.unwrap().contains("só vale"));
        let (_, a) = handle(Key::Char('R'), &at(2), &v);
        assert!(matches!(a, Some(Action::Request(RequestKind::Retry, Some(_)))));
        let (ui, a) = handle(Key::Char('r'), &at(1), &v);
        assert!(a.is_none() && ui.confirm.is_none());
        let (ui, a) = handle(Key::Char('a'), &at(5), &v);
        assert!(a.is_none() && ui.confirm.is_none());
        let (ui, _) = handle(Key::Char('a'), &at(1), &v);
        assert_eq!(ui.confirm, Some((RequestKind::Adopt, PrKey::new("o/r", 1))));
    }

    #[test]
    fn simple_actions() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s")]);
        assert!(matches!(handle(Key::Enter, &UiState::default(), &v).1, Some(Action::Focus(_))));
        assert!(matches!(handle(Key::Char('o'), &UiState::default(), &v).1, Some(Action::OpenUrl(..))));
        assert!(matches!(handle(Key::Char('l'), &UiState::default(), &v).1, Some(Action::ShowLog(_))));
        assert!(matches!(handle(Key::Char('s'), &UiState::default(), &v).1, Some(Action::Request(RequestKind::Sync, None))));
        assert!(matches!(handle(Key::Char('q'), &UiState::default(), &v).1, Some(Action::Quit)));
        assert!(matches!(handle(Key::CtrlC, &UiState::default(), &v).1, Some(Action::Quit)));
        let empty = view(vec![]);
        assert!(handle(Key::Enter, &UiState::default(), &empty).1.is_none());
    }

    #[test]
    fn async_message_does_not_alter_confirmation() {
        let v = view(vec![adopted(5)]);
        let ui = UiState { confirm: Some((RequestKind::Release, PrKey::new("o/r", 5))), message: Some("sync ok".into()), ..Default::default() };
        let (ui2, a) = handle(Key::Char('s'), &ui, &v);
        assert!(a.is_none());
        assert_eq!(ui2.confirm, ui.confirm);
        assert_eq!(ui2.message, ui.message, "mensagem do worker preservada");
        let (ui3, a) = handle(Key::Char('y'), &ui, &v);
        assert!(matches!(a, Some(Action::Request(RequestKind::Release, Some(ref k))) if k.number == 5));
        assert!(ui3.confirm.is_none());
    }

    #[test]
    fn entering_confirmation_does_not_write_message() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s")]);
        let (ui, _) = handle(Key::Char('a'), &UiState::default(), &v);
        assert!(ui.confirm.is_some() && ui.message.is_none());
        assert!(confirm_text(RequestKind::Adopt, &PrKey::new("o/r", 1)).contains("(y/n)"));
    }
}
