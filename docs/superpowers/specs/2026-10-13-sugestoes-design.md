# Início do trabalho, status de CI e meus PRs

Data: 2026-10-13 · Branch: `feat/sugestoes` · Revisão 2 (após review do Codex)

## Objetivo

Hoje o painel mostra a fila de review, mas não diz em quais PRs eu de fato comecei a trabalhar, nem como está o CI, nem o que eu mesmo abri. Muitos worktrees são criados e nunca usados, porque a revisão acontece direto no Linear. Os que importam são aqueles em que abri um Claude ou Codex.

O que entra:

1. Nos worktrees de review, a hora em que um agente (Claude ou Codex) abriu ali pela primeira vez.
2. Status de CI (rodando, passou ou falhou) nos PRs de review e nos meus.
3. A lista dos meus PRs abertos e quantos PRs criei nas últimas 24h.

## Premissas (confirmadas)

- Só os worktrees de review contam para "início do trabalho".
- "Iniciado" é informativo: não muda as regras de remoção nem de adoção.
- CI aparece só no painel e no `status`. Sem notificação.
- Meus PRs vêm só dos repos em `[[repos]]` do config.
- A lista mostra PRs abertos, com drafts marcados.
- O contador de 24h conta PRs criados nas últimas 24h, inclusive os já mergeados ou fechados.
- Meus PRs não ganham worktree nem setup.

## Princípio: tudo novo é metadado opcional

Nada desta mudança pode afetar o sync de reviews, a reconciliação, a carência ou a remoção de worktrees. Por isso:

- A query de reviews (`PENDING_QUERY`) e o `fetch_pending` **não mudam**. O CI vem de coletas separadas.
- As coletas novas rodam numa etapa própria do `tick`, **depois** do `poll_cycle` e dos efeitos dele. Nunca entre reconciliar e executar operações.
- Elas só escrevem campos informativos: `PrRecord.ci`, `PrRecord.work_started_at`, `PrRecord.work_agent` e `State.my_prs`. Não tocam `remote`, `phase`, `ownership`, `generation`, SHAs, `not_pending_since`, `notify_pending`, `State.repos` nem `auth_error_notified`.
- Falha numa coleta nova fica registrada no próprio dado (erro por repo) e no `daemon.log`. Nunca vira `RepoSnapshot::Failed`, alerta ou notificação.
- Meus PRs ficam em `State.my_prs`, nunca em `State.prs`.

Sobre a carência, para registro: hoje uma falha de sync suspende a remoção naquele ciclo, mas o relógio da carência (`not_pending_since`) continua correndo. Esta mudança não altera isso, e como a query de reviews fica igual, também não aumenta a chance de falha.

## Etapa de coleta no daemon

Um método novo `maybe_collect_extras` entra no `tick`, logo depois de `maybe_collect_reviews_today`. Roda quando passou `poll_interval_secs` desde a última coleta de extras ou quando chega um pedido de sync.

- Prazo total de **30 s** para a etapa inteira (agentes + GitHub), usando o `gh_json` com prazo que já existe. Prazo vencido no meio: o que já foi coletado é aplicado, o resto fica para o próximo ciclo.
- Cancela entre chamadas se o daemon estiver encerrando. Tentativa cancelada não grava nada.
- Grava o estado uma vez no fim.
- Ordem: agentes (local, barato), depois CI dos reviews, depois meus PRs.
- Com o contador "Feitas hoje" (até 90 s) e o `ensure` do painel (até 15 s), o pior tick fica em ~135 s, abaixo dos 150 s que o TUI espera antes de dizer "sem resposta do daemon".

## Início do trabalho

- `PrRecord` ganha `work_started_at: Option<DateTime<Utc>>` e `work_agent: Option<String>`, com `#[serde(default)]`.
- A trait `Herdr` ganha `list_agents(deadline) -> Result<Vec<AgentInfo>>`, implementada no `HerdrCli` com `herdr agent list` (mesma sessão do daemon). `AgentInfo { agent: String, cwd: Option<PathBuf>, foreground_cwd: Option<PathBuf> }`, lido de `result.agents`. Entradas sem caminho absoluto são ignoradas uma a uma.
- Só contam agentes `claude` e `codex`. Qualquer `agent_status` conta, porque o agente existir já é evidência de que o trabalho começou.
- Para cada registro (gerenciado ou adotado) sem `work_started_at`, procura um agente cujo `cwd` ou `foreground_cwd`, canonicalizado, seja o `path` canonicalizado do worktree ou esteja dentro dele (`Path::starts_with`, por componente). Assim `pr-157` não casa com `pr-1579` e `/tmp` casa com `/private/tmp`. Se houver mais de um, vale o primeiro em ordem alfabética do nome do agente.
- Ao achar, grava `now` e o nome. Depois disso o valor não muda.
- O herdr não informa quando o agente abriu. O horário é o da primeira coleta que o viu, normalmente até um `poll_interval_secs` de atraso, mais se o tick anterior demorou. Um agente aberto e fechado entre duas coletas não é detectado.
- O início pertence ao worktree físico: o `retry` que recria um worktree sumido (`Blocked` → `Creating`) limpa os dois campos. O `retry` de setup, a atualização de head e a adoção não limpam. A remoção concluída apaga o registro inteiro.
- Falha do `herdr agent list`: nada é atualizado, uma linha no `daemon.log`.

## CI

### Modelo

```rust
pub enum CiState { Pending, Success, Failure, #[serde(other)] Unknown }

pub struct Ci {
    pub sha: String,                         // commit do qual o rollup veio
    pub state: CiState,
    pub failed_check: Option<String>,
    pub finished_at: Option<DateTime<Utc>>,  // só com conjunto completo e terminado
    pub observed_at: DateTime<Utc>,
}
```

`#[serde(other)]` evita que um valor desconhecido num `state.json` futuro faça o arquivo inteiro ser isolado como corrompido.

### Fragmento GraphQL

Usado nas duas coletas (reviews e meus PRs):

```graphql
headRefOid
commits(last: 1) { nodes { commit { oid statusCheckRollup {
  state
  contexts(first: 50) {
    pageInfo { hasNextPage }
    nodes {
      __typename
      ... on CheckRun { name status conclusion completedAt }
      ... on StatusContext { context state createdAt }
    }
  }
} } } }
```

### Classificação

- `statusCheckRollup.state`: `SUCCESS` vira `Success`. `FAILURE` e `ERROR` viram `Failure`. `PENDING` e `EXPECTED` viram `Pending`. Rollup nulo (PR sem checks) ou estado desconhecido viram `ci: None`.
- Check que falhou: `CheckRun` com `conclusion` em `FAILURE`, `TIMED_OUT`, `CANCELLED`, `ACTION_REQUIRED`, `STARTUP_FAILURE` ou `STALE`; `StatusContext` com `state` `FAILURE` ou `ERROR`. `SUCCESS`, `NEUTRAL`, `SKIPPED` e conclusão nula não são falha. Vale o primeiro na ordem devolvida pelo GitHub. O nome é `name` (CheckRun) ou `context` (StatusContext).
- `finished_at`: só quando o estado não é `Pending`, `hasNextPage` é falso e nenhum `CheckRun` tem `status` diferente de `COMPLETED`. É o maior entre `completedAt` dos CheckRuns e `createdAt` dos StatusContexts. Fora disso fica `None` e o painel mostra o ícone sem idade.
- Detalhe malformado (contexto sem nome, tipo desconhecido) é ignorado sem descartar o `state`. Só rollup ausente ou com `state` inválido resulta em `None`.

### CI dos PRs de review

- Uma query por repo com aliases, um por registro do repo em `State.prs` (gerenciado ou adotado, inclusive em Saindo): `repository(owner, name) { p123: pullRequest(number: 123) { <fragmento> } ... }`, em lotes de até 20 PRs.
- Cobre os PRs em Saindo e Adotados, que a busca de reviews não traz mais.
- Lote com erro de transporte ou GraphQL: os registros daquele lote mantêm o `ci` anterior. PR que voltou nulo (fechado e apagado, sem acesso) também mantém.
- O CI é sempre o do **head remoto** do PR (`headRefOid`). O painel só mostra o CI quando `ci.sha == observed_head_sha`. Se o head mudou e o CI novo ainda não chegou, mostra `?`, então um head novo nunca herda o verde do anterior. O worktree local pode estar em outro commit (adotado, update que falhou). O CI não diz nada sobre o conteúdo local.

## Meus PRs

### Coleta

- O viewer é obtido no início da etapa com `fetch_viewer` e fica fixo durante a coleta. As buscas usam `author:<viewer>`.
- Se o viewer for diferente de `State.my_prs.viewer`, todo o `my_prs` é zerado **antes** de aplicar qualquer resultado, mesmo que os repos novos falhem. Dados de contas diferentes nunca se misturam. Se o `fetch_viewer` falhar, nada muda.
- Janela fixa por coleta: `since = as_of − 24h`, com `as_of` capturado no início.
- Por repo, duas chamadas:
  - **Lista:** `search(query: "repo:R is:pr is:open author:V", type: ISSUE, first: 50, after: $after)` com `repository(owner, name) { id }` para checar acesso, `issueCount`, `pageInfo` e, por PR, `number title url isDraft createdAt` + fragmento de CI. Paginação com detecção de cursor repetido, deduplicação por número e limite de 20 páginas. `issueCount` acima de 1.000 é erro.
  - **Contador:** `search(query: "repo:R is:pr author:V created:>=<since ISO 8601>", type: ISSUE, first: 1) { issueCount }`.
- Resultado por repo é atômico: lista e contador juntos, ou falha do repo inteiro. Falha mantém o último resultado bom daquele repo e grava o erro.

### Estado

```rust
pub struct MyPrs {
    pub viewer: Option<String>,
    pub repos: BTreeMap<String, MyPrsRepo>,  // último resultado bom por repo
    pub errors: BTreeMap<String, String>,    // erro da última tentativa por repo
}
pub struct MyPrsRepo { pub prs: Vec<MyPr>, pub opened_24h: u64, pub as_of: DateTime<Utc> }
pub struct MyPr { pub number: u64, pub title: String, pub url: String, pub draft: bool,
                  pub created_at: DateTime<Utc>, pub head_sha: String, pub ci: Option<Ci> }
```

Repos que saíram do config são removidos de `repos` e `errors` na coleta seguinte. O config só é relido quando o daemon reinicia, como já acontece com o resto. O painel e o `status` também filtram pela config que carregaram.

## Painel

### Contadores

Quatro áreas na linha de contadores: **Pendentes** (14), **Feitas hoje** (16), **Abertos 24h** (16) e o detalhe, que hoje mostra a idade e o erro do "Feitas hoje" e passa a ocupar o que sobra.

"Abertos 24h" mostra a soma de `opened_24h` dos repos do config:
- `–` se nenhum repo tem resultado;
- `N?` se algum repo do config está sem resultado, com erro na última tentativa ou com `as_of` mais velho que 3 × `poll_interval_secs`;
- `N` caso contrário.

Erros de meus PRs entram na lista de erros do rodapé como `meus PRs · o/r: <erro>`.

### Identidade das linhas

Hoje toda linha é identificada por `PrKey`. Passa a ser `RowId`:

```rust
pub enum RowId { Review(PrKey), Mine(PrKey) }
```

`Row.key` vira `Row.id`. `UiState.selected` e `UiState.order` passam a guardar `RowId`, e `reselect` e a navegação também. `confirm` continua com `PrKey`, porque só linhas de review aceitam pedidos. Um PR que aparece nas duas listas vira duas linhas distintas, sem colisão.

### Seções e ordem

```
  PR       Estado              CI          Início Autor     Título
 Iniciados
▸ #15791 ● pronto              ✓ 12min     14:32  fulano    Buyer own_setting…
  #15688 ● pronto              ◐           11:05  beltrano  Ajusta webhook…
 Pendentes
  #15802 ● pronto              ✗ rspec            ciclano   Novo filtro…
  #15799 ◐ preparando          ◐                  fulano    Migra tags…
 Saindo
  #15650 ● pronto · sai em 9min ✓          10:12  ...
 Adotados
  ...
 Meus PRs
  #15810 ◐ aberto              ◐                  você      Corrige import…
  #15790 ✓ rascunho            ✓ 2h               você      Refatora…
```

- `Section` passa a ser `Started, Pending, NoSync, Leaving, Adopted, Mine`, nesta ordem. Todas as seções têm título, inclusive **Pendentes**.
- **Iniciados**: registros que `section_of` colocaria em `Pending` e têm `work_started_at`. Ordenados pelo início mais recente, depois pela chave.
- Um PR iniciado que sai da fila vai para **Saindo**. O horário de início continua na coluna Início em qualquer seção.
- As demais seções de review mantêm a ordem atual (fase, `first_seen_at`, chave).
- **Meus PRs**: ordenados pelo `created_at` mais recente, depois pela chave. A coluna Autor mostra `você`. Estado `aberto` ou `rascunho`. Ícone da linha vem do CI: `◐` rodando, `✓` passou, `✗` falhou, `·` sem CI.

### Colunas

CI e Início viram colunas próprias, em vez de sufixos no texto do estado, para não competir com a contagem regressiva da carência nem com motivos de falha.

- **CI** (até 10 colunas): `◐`, `✓ <idade>` (idade desde `finished_at`, omitida quando `None`), `✗ <check>` (cortado), `?` (sem dado ou SHA diferente do head), vazio para PR sem checks.
- **Início** (5 colunas): `HH:MM` em São Paulo quando é de hoje, `dd/mm` quando é de outro dia, vazio sem início. O nome do agente aparece no `status` da CLI.
- Em largura menor, quem encolhe é o título, depois o autor. O estado, CI e Início mantêm a largura. Em 60 colunas, o estado completo `pronto · sai em 15min` ainda aparece.

### Teclas e ações

- `Eligibility` ganha `focus` (renomeia o `open` atual: focar workspace) e `log`.
- Linhas de review: comportamento atual.
- Linhas de Meus PRs: `enter` e `o` abrem o PR no navegador (`Action::OpenUrl`). `l`, `R`, `a` e `r` mostram uma mensagem e não fazem nada.
- `Action::OpenUrl` passa a carregar `RowId`. O worker relê o estado e procura o PR na coleção certa (`State.prs` ou `State.my_prs`). PR que sumiu é recusado com a mensagem atual. `Focus` e `ShowLog` continuam só com `PrKey` de review.
- `first-ready` não muda: considera só `State.prs`.

### CLI

`herdr-reviewq status` ganha:
- em cada PR de review, ` · CI <estado>` e ` · iniciado HH:MM <agente>` quando houver;
- a linha `Abertos 24h: N` (mesma regra de `–` e `?`) logo depois de "Feitas hoje";
- erros de meus PRs como `! meus PRs · o/r: <erro>`;
- uma seção `Meus PRs` com uma linha por PR.

## Testes

Padrão do repo: `FakeRunner`, `FakeHerdr` e fixtures JSON.

- **Parse de CI:** os cinco estados do rollup, rollup nulo, estado desconhecido, falha vinda de CheckRun e de StatusContext, cada conclusão de falha e as que não são falha, `finished_at` com CheckRun e StatusContext, `hasNextPage` verdadeiro sem `finished_at`, CheckRun em andamento sem `finished_at`, detalhe malformado que mantém o `state`.
- **CI dos reviews:** aliases por lote, lote com erro mantém o `ci` anterior, PR nulo mantém, registros adotados e em Saindo recebem CI, `ci.sha` diferente do head mostra `?`.
- **Meus PRs:** lista, draft, paginação, cursor repetido, `issueCount` acima de 1.000, repo sem acesso, contador, falha parcial mantém o resultado anterior do repo, troca de viewer zera antes de aplicar, `fetch_viewer` falhando não muda nada, repo fora do config é removido.
- **Agentes:** parse do `herdr agent list` real, `cwd` igual ao worktree, subdiretório, `foreground_cwd`, prefixo parecido não casa, caminho com symlink casa, agente fora da allowlist ignorado, valor estável entre coletas, falha do herdr sem efeito, `retry` com worktree sumido limpa, `retry` de setup não limpa.
- **Isolamento:** com a etapa de extras falhando por inteiro (gh e herdr), um ciclo completo produz as mesmas operações, `generation`, `phase`, `ownership`, `not_pending_since` e notificações de um ciclo sem extras. Uma mudança de CI não incrementa `generation` nem dispara `Update`. Extras cancelados pelo encerramento não gravam nada.
- **State:** `state.json` sem os campos novos carrega. `CiState` desconhecido vira `Unknown` sem isolar o arquivo. Roundtrip dos tipos novos.
- **Modelo do TUI:** ordem das seções, Iniciados ordenados por início, iniciado que vai para Saindo, `RowId` distinto para o mesmo PR nas duas listas, `reselect` com `RowId`, elegibilidade em Meus PRs, contador `–`/`N?`/`N`.
- **Teclas e worker:** `enter` e `o` em Meus PRs geram `OpenUrl(Mine)`, `l`/`R`/`a`/`r` não geram ação, o worker abre a URL relida de `my_prs` e recusa PR sumido.
- **Render:** título "Pendentes", colunas CI e Início, contador "Abertos 24h", conteúdo visível em 60 colunas com nome de check longo, carência e início.
- **`first-ready`:** testes atuais continuam passando sem mudança.

## Fora de escopo

- Notificação de CI.
- Usar o início do trabalho para proteger o worktree da remoção.
- Worktree ou setup para os meus PRs.
- PRs fora dos repos do config.
- Review decision (aprovado, mudanças pedidas) nos meus PRs.
- Recarregar o config sem reiniciar o daemon.
