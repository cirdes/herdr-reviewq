//! Pilhas de PRs (stacked PRs): um PR está empilhado quando sua base é a branch de outro PR
//! aberto do mesmo repo. Os nós vêm dos PRs abertos coletados pelo daemon (`state.open_prs`),
//! completados pelos PRs da fila; sem coleta ainda, só a fila entra e a pilha fica parcial.
//! PR de fork não entra (a coleta já o descarta) e PR cuja head é a branch padrão do repo
//! (release `main` → `production`) não abre pilha: senão todo PR para `main` viraria filho dele.

use crate::state::{PrKey, Remote, State};
use std::collections::{BTreeMap, BTreeSet};

/// Rótulo de quem está fora de pilha.
pub const NONE: &str = "Não";

struct Node {
    number: u64,
    head: String,
    base: String,
}

/// Rótulo por PR conhecido: "#raiz i/n" (pré-ordem a partir do PR de baixo, filhos por número)
/// ou `NONE`. Entram os PRs abertos coletados e os da fila. Quando os dois divergem, vale o PR
/// pendente da fila, que o daemon atualiza a cada sync; a coleta é mais lenta e, se falhar, fica
/// parada. PR da fila que não está pendente não é mais atualizado, então aí vale a coleta.
pub fn labels(state: &State) -> BTreeMap<PrKey, String> {
    let mut nodes: BTreeMap<&str, BTreeMap<u64, Node>> = BTreeMap::new();
    for (repo, open) in &state.open_prs.repos {
        for p in &open.prs {
            let node = Node { number: p.number, head: p.head_ref.clone(), base: p.base_ref.clone() };
            nodes.entry(repo).or_default().insert(p.number, node);
        }
    }
    for r in state.prs.values() {
        let node = Node { number: r.key.number, head: r.head_ref.clone(), base: r.base_ref.clone() };
        let repo = nodes.entry(&r.key.repo).or_default();
        if r.remote == Remote::Pending && !r.base_ref.is_empty() {
            repo.insert(r.key.number, node);
        } else {
            repo.entry(r.key.number).or_insert(node);
        }
    }
    let mut out = BTreeMap::new();
    for (repo, nodes) in &nodes {
        let default_branch = state.open_prs.repos.get(*repo).and_then(|o| o.default_branch.as_deref());
        for (n, label) in repo_labels(nodes, default_branch) {
            out.insert(PrKey::new(repo, n), label);
        }
    }
    out
}

fn repo_labels(nodes: &BTreeMap<u64, Node>, default_branch: Option<&str>) -> BTreeMap<u64, String> {
    let by_head: BTreeMap<&str, &Node> =
        nodes.values().filter(|n| Some(n.head.as_str()) != default_branch).map(|n| (n.head.as_str(), n)).collect();
    let parent = |n: &Node| by_head.get(n.base.as_str()).filter(|p| p.number != n.number).copied();
    let mut children: BTreeMap<u64, Vec<&Node>> = BTreeMap::new();
    for n in nodes.values() {
        if let Some(p) = parent(n) {
            children.entry(p.number).or_default().push(n);
        }
    }
    let mut labels: BTreeMap<u64, String> = nodes.keys().map(|n| (*n, NONE.to_string())).collect();
    for root in nodes.values().filter(|n| parent(n).is_none() && children.contains_key(&n.number)) {
        let mut order = vec![];
        let mut seen = BTreeSet::new();
        let mut todo = vec![root];
        while let Some(n) = todo.pop() {
            if !seen.insert(n.number) {
                continue;
            }
            order.push(n.number);
            let mut kids = children.get(&n.number).cloned().unwrap_or_default();
            kids.sort_by_key(|k| std::cmp::Reverse(k.number));
            todo.extend(kids);
        }
        let total = order.len();
        for (i, n) in order.into_iter().enumerate() {
            labels.insert(n, format!("#{} {}/{total}", root.number, i + 1));
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{OpenPr, OpenPrsRepo, PrRecord};
    use chrono::Utc;

    fn rec(repo: &str, n: u64, head: &str, base: &str) -> PrRecord {
        let mut r = PrRecord::fixture(repo, n, head, "s");
        r.base_ref = base.into();
        r
    }

    fn open(n: u64, head: &str, base: &str) -> OpenPr {
        OpenPr { number: n, head_ref: head.into(), base_ref: base.into() }
    }

    fn with_open(s: &mut State, repo: &str, prs: Vec<OpenPr>) {
        s.open_prs.repos.insert(repo.into(), OpenPrsRepo { prs, default_branch: Some("main".into()), as_of: Utc::now() });
    }

    fn label(s: &State, repo: &str, n: u64) -> String {
        labels(s).get(&PrKey::new(repo, n)).cloned().unwrap_or_else(|| panic!("sem rótulo para {repo}#{n}"))
    }

    #[test]
    fn queue_only_chain_tree_and_other_repo() {
        let mut s = State::default();
        // 1 ← 2 ← 3 e 1 ← 5 (árvore a partir de 1); 4 sozinho; o/x#9 usa os mesmos nomes de
        // branch, mas em outro repo, e não entra na pilha
        for r in [
            rec("o/r", 1, "feat/a", "main"),
            rec("o/r", 2, "feat/b", "feat/a"),
            rec("o/r", 3, "feat/c", "feat/b"),
            rec("o/r", 5, "feat/e", "feat/a"),
            rec("o/r", 4, "feat/d", "main"),
            rec("o/x", 9, "feat/b", "feat/a"),
        ] {
            s.insert(r);
        }
        assert_eq!(label(&s, "o/r", 1), "#1 1/4");
        assert_eq!(label(&s, "o/r", 2), "#1 2/4");
        assert_eq!(label(&s, "o/r", 3), "#1 3/4");
        assert_eq!(label(&s, "o/r", 5), "#1 4/4");
        assert_eq!(label(&s, "o/r", 4), NONE);
        assert_eq!(label(&s, "o/x", 9), NONE);
    }

    #[test]
    fn open_prs_keep_numbering_when_part_of_the_stack_left_the_queue() {
        let chain = || vec![open(1, "feat/a", "main"), open(2, "feat/b", "feat/a"), open(3, "feat/c", "feat/b")];
        // o de baixo já foi revisado: só 2 e 3 na fila
        let mut s = State::default();
        s.insert(rec("o/r", 2, "feat/b", "feat/a"));
        s.insert(rec("o/r", 3, "feat/c", "feat/b"));
        with_open(&mut s, "o/r", chain());
        assert_eq!(label(&s, "o/r", 2), "#1 2/3");
        assert_eq!(label(&s, "o/r", 3), "#1 3/3");
        assert_eq!(label(&s, "o/r", 1), "#1 1/3", "PR fora da fila também ganha rótulo (Meus PRs)");
        // o do meio saiu da fila: a cadeia continua inteira
        let mut s = State::default();
        s.insert(rec("o/r", 1, "feat/a", "main"));
        s.insert(rec("o/r", 3, "feat/c", "feat/b"));
        with_open(&mut s, "o/r", chain());
        assert_eq!(label(&s, "o/r", 1), "#1 1/3");
        assert_eq!(label(&s, "o/r", 3), "#1 3/3");
        // sem coleta, só a fila: o de cima fica sozinho
        let mut s = State::default();
        s.insert(rec("o/r", 2, "feat/b", "feat/a"));
        assert_eq!(label(&s, "o/r", 2), NONE);
    }

    #[test]
    fn stale_open_list_is_completed_by_the_queue() {
        let mut s = State::default();
        with_open(&mut s, "o/r", vec![open(1, "feat/a", "main")]);
        s.insert(rec("o/r", 2, "feat/b", "feat/a"));
        assert_eq!(label(&s, "o/r", 1), "#1 1/2");
        assert_eq!(label(&s, "o/r", 2), "#1 2/2");
    }

    #[test]
    fn release_pr_from_default_branch_does_not_adopt_every_pr() {
        let mut s = State::default();
        with_open(&mut s, "o/r", vec![open(1, "main", "production"), open(2, "feat/a", "main"), open(3, "feat/b", "feat/a")]);
        assert_eq!(label(&s, "o/r", 1), NONE);
        assert_eq!(label(&s, "o/r", 2), "#2 1/2");
        assert_eq!(label(&s, "o/r", 3), "#2 2/2");
    }

    #[test]
    fn pending_queue_record_beats_stale_collection() {
        // o 1 foi mergeado e o GitHub mudou a base do 2 para main; a coleta ainda é a de antes
        let mut s = State::default();
        with_open(&mut s, "o/r", vec![open(1, "feat/a", "main"), open(2, "feat/b", "feat/a")]);
        let mut r = rec("o/r", 2, "feat/b", "main");
        r.remote = Remote::Pending;
        s.insert(r);
        assert_eq!(label(&s, "o/r", 2), NONE);
        // fora de pendente o registro da fila para de ser atualizado: vale a coleta
        s.get_mut(&PrKey::new("o/r", 2)).unwrap().remote = Remote::NotPending;
        assert_eq!(label(&s, "o/r", 2), "#1 2/2");
    }
}
