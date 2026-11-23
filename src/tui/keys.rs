use crate::requests::RequestKind;
use crate::state::PrKey;
use crate::tui::model::{kind_label, Row, RowId, UiState, View};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Esc,
    Backspace,
    CtrlC,
    /// Limpa a busca enquanto digita (hábito de terminal); fora da busca não faz nada.
    CtrlU,
    Char(char),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Focus(PrKey),
    /// Enter num PR seu: foca o worktree da branch, criando se não houver.
    OpenMine(PrKey),
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
        RequestKind::Sync | RequestKind::ReleaseAll => true,
    }
}

fn not_eligible_msg(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Retry => "retry only applies to a failed setup or one blocked without a worktree",
        RequestKind::Adopt => "you can only adopt a worktree managed by the daemon",
        RequestKind::Release => "you can only release an adopted worktree",
        RequestKind::Sync | RequestKind::ReleaseAll => "",
    }
}

pub fn confirm_text(kind: RequestKind, key: &PrKey) -> String {
    match kind {
        RequestKind::Adopt => format!("adopt {key}? protects it from automatic cleanup and cancels any running setup (y/n)"),
        RequestKind::Release => {
            format!("release {key}? removes the worktree; if the PR is still pending, it only comes back with a new review request (y/n)")
        }
        other => format!("{} {key}? (y/n)", kind_label(other)),
    }
}

pub fn release_all_text(n: usize) -> String {
    let what = if n == 1 { "the 1 adopted PR".to_string() } else { format!("all {n} adopted PRs") };
    format!("release {what}? removes their worktrees; any with uncommitted or unpushed work is kept (y/n)")
}

fn move_selection(mut next: UiState, key: Key, index: Option<usize>, view: &View) -> UiState {
    if let Some(i) = index {
        let j = if key == Key::Up { i.saturating_sub(1) } else { (i + 1).min(view.rows.len() - 1) };
        next.selected = Some(view.rows[j].id.clone());
    }
    next
}

pub fn handle(key: Key, ui: &UiState, view: &View) -> (UiState, Option<Action>) {
    let mut next = ui.clone();

    if ui.confirm_all {
        return match key {
            Key::Char('y') | Key::Char('Y') => {
                next.confirm_all = false;
                if view.adopted > 0 {
                    (next, Some(Action::Request(RequestKind::ReleaseAll, None)))
                } else {
                    next.message = Some("no adopted PRs left; request canceled".into());
                    (next, None)
                }
            }
            Key::Char('n') | Key::Char('N') | Key::Esc => {
                next.confirm_all = false;
                next.message = Some("canceled".into());
                (next, None)
            }
            Key::CtrlC => (next, Some(Action::Quit)),
            _ => (ui.clone(), None),
        };
    }

    if let Some((kind, target)) = ui.confirm.clone() {
        return match key {
            Key::Char('y') | Key::Char('Y') => {
                next.confirm = None;
                match view.row(&RowId::Review(target.clone())) {
                    Some(row) if eligible(row, kind) => (next, Some(Action::Request(kind, Some(target)))),
                    _ => {
                        next.message = Some(format!("{target} changed state; request canceled"));
                        (next, None)
                    }
                }
            }
            Key::Char('n') | Key::Char('N') | Key::Esc => {
                next.confirm = None;
                next.message = Some("canceled".into());
                (next, None)
            }
            Key::CtrlC => (next, Some(Action::Quit)),
            _ => (ui.clone(), None),
        };
    }

    let selected = ui.selected_row(view);
    let index = selected.and_then(|r| view.rows.iter().position(|x| x.id == r.id));

    // Digitando a busca: toda letra vai para o filtro (inclusive `q`, `s`, `a`...); só setas, enter,
    // esc, backspace, ctrl+u e ctrl+c mantêm o sentido. A mensagem do worker não é apagada aqui:
    // o rodapé mostra o prompt, então ela fica no topo até a busca terminar.
    if ui.typing {
        match key {
            Key::Char(c) => next.filter.push(c),
            Key::Backspace => {
                next.filter.pop();
            }
            Key::CtrlU => next.filter.clear(),
            Key::Enter => {
                next.typing = false;
                // filtro só com espaços não filtra nada; não pode deixar um `esc` "preso" nele
                next.filter = next.filter.trim().to_string();
            }
            Key::Esc => {
                next.typing = false;
                next.filter.clear();
            }
            Key::CtrlC => return (next, Some(Action::Quit)),
            Key::Up | Key::Down => return (move_selection(next, key, index, view), None),
        }
        return (next, None);
    }

    next.message = None;
    match key {
        Key::Up | Key::Char('k') => (move_selection(next, Key::Up, index, view), None),
        Key::Down | Key::Char('j') => (move_selection(next, Key::Down, index, view), None),
        Key::Char('/') => {
            next.typing = true;
            (next, None)
        }
        // com busca ativa, `esc` limpa a busca em vez de sair
        Key::Esc if !ui.filter.trim().is_empty() => {
            next.filter.clear();
            (next, None)
        }
        Key::Char('q') | Key::Esc | Key::CtrlC => (next, Some(Action::Quit)),
        Key::Char('s') => (next, Some(Action::Request(RequestKind::Sync, None))),
        Key::Char('X') => {
            if view.adopted > 0 {
                // a confirmação é desenhada a partir de `confirm_all` (release_all_text)
                next.confirm_all = true;
            } else {
                next.message = Some("no adopted PRs to release".into());
            }
            (next, None)
        }
        _ => {
            let Some(row) = selected else { return (next, None) };
            let Some(key_pr) = row.id.review() else {
                // meus PRs: enter vai para o worktree (criado se faltar), `o` abre no navegador
                match key {
                    Key::Enter => {
                        next.message = Some(format!("opening the worktree of {}…", row.id.key()));
                        return (next, Some(Action::OpenMine(row.id.key().clone())));
                    }
                    Key::Char('o') => return (next, Some(Action::OpenUrl(row.id.clone(), row.url.clone()))),
                    Key::Char('l' | 'R' | 'a' | 'r') => next.message = Some("only for review PRs".into()),
                    _ => {}
                }
                return (next, None);
            };
            match key {
                Key::Enter if row.eligible.focus => (next, Some(Action::Focus(key_pr.clone()))),
                Key::Enter => {
                    next.message = Some("this PR is being created or removed".into());
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
    fn enter_on_mine_opens_worktree_and_o_opens_url() {
        let v = view_with_mine(vec![PrRecord::fixture("o/r", 9, "x", "s")], &[9]);
        let (ui, a) = handle(Key::Enter, &mine_at(9), &v);
        assert_eq!(a, Some(Action::OpenMine(PrKey::new("o/r", 9))));
        assert!(ui.message.unwrap().contains("o/r#9"));
        let want = Action::OpenUrl(RowId::Mine(PrKey::new("o/r", 9)), "https://github.com/o/r/pull/9".into());
        assert_eq!(handle(Key::Char('o'), &mine_at(9), &v).1, Some(want));
    }

    #[test]
    fn log_retry_adopt_release_on_mine_do_nothing() {
        let v = view_with_mine(vec![], &[9]);
        for k in ['l', 'R', 'a', 'r'] {
            let (ui, a) = handle(Key::Char(k), &mine_at(9), &v);
            assert!(a.is_none(), "{k}");
            assert!(ui.confirm.is_none(), "{k}");
            assert!(ui.message.as_deref().unwrap_or("").contains("only for review PRs"), "{k}: {:?}", ui.message);
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
        assert!(a.is_none() && ui2.confirm.is_none() && ui2.message.unwrap().contains("canceled"));
        let now_managed = view(vec![PrRecord::fixture("o/r", 5, "x", "s")]);
        let (_, a) = handle(Key::Char('y'), &ui, &now_managed);
        assert!(a.is_none());
        let (ui3, a) = handle(Key::Esc, &ui, &v);
        assert!(a.is_none() && ui3.confirm.is_none());
        let (ui4, a) = handle(Key::Char('s'), &ui, &v);
        assert!(a.is_none() && ui4.confirm.is_some(), "outras teclas são ignoradas na confirmação");
    }

    #[test]
    fn release_all_asks_first_and_sends_one_request() {
        let v = view(vec![adopted(5), adopted(6), PrRecord::fixture("o/r", 1, "x", "s")]);
        assert_eq!(v.adopted, 2);
        let (ui, a) = handle(Key::Char('X'), &UiState::default(), &v);
        assert!(a.is_none() && ui.confirm_all && ui.confirm.is_none());
        let (ui2, a) = handle(Key::Char('s'), &ui, &v);
        assert!(a.is_none() && ui2.confirm_all, "other keys are ignored while confirming");
        let (ui2, a) = handle(Key::Char('n'), &ui, &v);
        assert!(a.is_none() && !ui2.confirm_all);
        let (ui2, a) = handle(Key::Char('y'), &ui, &v);
        assert_eq!(a, Some(Action::Request(RequestKind::ReleaseAll, None)));
        assert!(!ui2.confirm_all);
        // os adotados sumiram entre o `X` e o `y`
        let (ui2, a) = handle(Key::Char('y'), &ui, &view(vec![]));
        assert!(a.is_none() && ui2.message.unwrap().contains("no adopted"));
    }

    #[test]
    fn release_all_without_adopted_only_warns() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s")]);
        let (ui, a) = handle(Key::Char('X'), &UiState::default(), &v);
        assert!(a.is_none() && !ui.confirm_all && ui.message.unwrap().contains("no adopted"));
        assert!(release_all_text(1).contains("the 1 adopted PR?") && release_all_text(3).contains("all 3 adopted PRs?"));
    }

    #[test]
    fn eligibility_is_enforced() {
        let mut failed = PrRecord::fixture("o/r", 2, "x", "s");
        failed.phase = Phase::Failed { step: "a".into(), reason: "b".into() };
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s"), failed, adopted(5)]);
        let at = |n| UiState { selected: Some(RowId::Review(PrKey::new("o/r", n))), ..Default::default() };
        let (ui, a) = handle(Key::Char('R'), &at(1), &v);
        assert!(a.is_none() && ui.message.unwrap().contains("only applies"));
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

    fn type_str(mut ui: UiState, v: &View, s: &str) -> UiState {
        for c in s.chars() {
            let (next, a) = handle(Key::Char(c), &ui, v);
            assert!(a.is_none(), "{c} virou ação durante a busca");
            ui = next;
        }
        ui
    }

    #[test]
    fn slash_starts_search_and_letters_do_not_trigger_actions() {
        let v = view(vec![adopted(5)]);
        let (ui, a) = handle(Key::Char('/'), &UiState::default(), &v);
        assert!(a.is_none() && ui.typing);
        // q, s, a, r seriam sair, sync, adotar e liberar fora da busca
        let ui = type_str(ui, &v, "qsar");
        assert_eq!(ui.filter, "qsar");
        assert!(ui.confirm.is_none() && ui.typing);
    }

    #[test]
    fn backspace_enter_and_esc_while_typing() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s")]);
        let ui = type_str(UiState { typing: true, ..Default::default() }, &v, "eng-1");
        let (ui, _) = handle(Key::Backspace, &ui, &v);
        assert_eq!(ui.filter, "eng-");
        let (kept, a) = handle(Key::Enter, &ui, &v);
        assert!(a.is_none() && !kept.typing && kept.filter == "eng-", "enter mantém o filtro");
        let (cleared, a) = handle(Key::Esc, &ui, &v);
        assert!(a.is_none() && !cleared.typing && cleared.filter.is_empty(), "esc na digitação limpa");
        assert_eq!(handle(Key::CtrlC, &ui, &v).1, Some(Action::Quit));
    }

    #[test]
    fn arrows_move_while_typing() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s"), PrRecord::fixture("o/r", 2, "x", "s")]);
        let (ui, a) = handle(Key::Down, &UiState { typing: true, ..Default::default() }, &v);
        assert!(a.is_none() && ui.typing);
        assert_eq!(ui.selected, Some(RowId::Review(PrKey::new("o/r", 2))));
    }

    #[test]
    fn confirmation_survives_refresh_that_unmatches_the_filter() {
        // `a` com a busca "PR 5"; o título muda numa releitura e o PR deixa de casar antes do `y`
        let ui = UiState { filter: "PR 5".into(), selected: Some(RowId::Review(PrKey::new("o/r", 5))), ..Default::default() };
        let (ui, _) = handle(Key::Char('a'), &ui, &view(vec![PrRecord::fixture("o/r", 5, "x", "s")]));
        assert!(ui.confirm.is_some());
        let mut renamed = PrRecord::fixture("o/r", 5, "x", "s");
        renamed.title = "outro título".into();
        let mut s = State::default();
        s.repos.insert("o/r".into(), RepoStatus { ok: true, last_sync: Some(Utc::now()), ..Default::default() });
        s.insert(renamed);
        let cfg = ViewConfig { repos: vec!["o/r".into()], poll_interval_secs: 60, remove_grace: chrono::Duration::minutes(15) };
        let v = View::build(Some(&s), &ui, &cfg, &UiFacts::default(), Utc::now());
        let (_, a) = handle(Key::Char('y'), &ui, &v);
        assert_eq!(a, Some(Action::Request(RequestKind::Adopt, Some(PrKey::new("o/r", 5)))));
    }

    #[test]
    fn esc_with_active_filter_clears_instead_of_quitting() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s")]);
        let ui = UiState { filter: "x".into(), ..Default::default() };
        let (ui, a) = handle(Key::Esc, &ui, &v);
        assert!(a.is_none() && ui.filter.is_empty());
        assert_eq!(handle(Key::Esc, &ui, &v).1, Some(Action::Quit));
        // com filtro mantido, as ações voltam a valer e `/` reabre a edição sem perder o texto
        let ui = UiState { filter: "x".into(), ..Default::default() };
        assert!(matches!(handle(Key::Enter, &ui, &v).1, Some(Action::Focus(_))));
        let (ui, _) = handle(Key::Char('/'), &ui, &v);
        assert!(ui.typing && ui.filter == "x");
    }

    #[test]
    fn worker_message_survives_typing() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s")]);
        let ui = UiState { typing: true, message: Some("sync falhou".into()), ..Default::default() };
        let ui = type_str(ui, &v, "ab");
        assert_eq!(ui.message.as_deref(), Some("sync falhou"));
        let (ui, _) = handle(Key::Enter, &ui, &v);
        assert_eq!(ui.message.as_deref(), Some("sync falhou"), "aparece no rodapé ao sair da digitação");
        let (ui, _) = handle(Key::Down, &ui, &v);
        assert!(ui.message.is_none(), "fora da busca a próxima tecla apaga, como antes");
    }

    #[test]
    fn ctrl_u_clears_search_and_is_ignored_outside() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s")]);
        let ui = type_str(UiState { typing: true, ..Default::default() }, &v, "abc");
        let (ui, a) = handle(Key::CtrlU, &ui, &v);
        assert!(a.is_none() && ui.filter.is_empty() && ui.typing);
        let (ui, a) = handle(Key::CtrlU, &UiState::default(), &v);
        assert!(a.is_none() && ui.confirm.is_none());
    }

    #[test]
    fn blank_filter_does_not_hold_esc() {
        let v = view(vec![PrRecord::fixture("o/r", 1, "x", "s")]);
        let ui = type_str(UiState { typing: true, ..Default::default() }, &v, "  ");
        let (ui, _) = handle(Key::Enter, &ui, &v);
        assert!(ui.filter.is_empty());
        assert_eq!(handle(Key::Esc, &ui, &v).1, Some(Action::Quit));
        // mesmo um filtro em branco vindo de outro caminho não segura o `esc`
        let ui = UiState { filter: "  ".into(), ..Default::default() };
        assert_eq!(handle(Key::Esc, &ui, &v).1, Some(Action::Quit));
    }
}
