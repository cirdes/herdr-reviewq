# Início do trabalho, status de CI e meus PRs

Data: 2026-10-13 · Branch: `feat/sugestoes`

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
- A lista mostra PRs abertos, com drafts marcados como `[draft]`.
- O contador de 24h conta PRs criados nas últimas 24h, inclusive os já mergeados ou fechados.
- Meus PRs não ganham worktree nem setup.

## Arquitetura

Tudo é coletado pelo daemon no ciclo de poll que já existe e gravado no `state.json`. O TUI e o `status` só leem o estado. O início do trabalho sobrevive a reinícios do daemon e do painel, e o GitHub é consultado de um lugar só.

### Início do trabalho

- `PrRecord` ganha `work_started_at: Option<DateTime<Utc>>` e `work_agent: Option<String>`, ambos com `#[serde(default)]`.
- A trait `Herdr` ganha `list_agents() -> Result<Vec<AgentInfo>>`, implementada com `herdr agent list`. `AgentInfo` carrega `agent`, `cwd` e `foreground_cwd`.
- A cada ciclo, para cada registro sem `work_started_at`, o daemon procura um agente cujo `cwd` ou `foreground_cwd` seja o `path` do worktree ou esteja dentro dele. A comparação é por componente de caminho (`Path::starts_with`), para que `pr-157` não case com `pr-1579`. Se achar, grava `now` e o nome do agente. Depois disso o valor não muda.
- O herdr não informa quando o agente abriu, então o horário é o do primeiro poll que o viu (erro de até `poll_interval_secs`). Um agente aberto e fechado entre dois polls não é detectado.
- Worktree removido e recriado gera registro novo, com início vazio.
- Falha do `herdr agent list`: o ciclo segue sem atualizar nada, sem alerta.

### CI

- Tipo novo `Ci { state: CiState, failed_check: Option<String>, finished_at: Option<DateTime<Utc>> }`, com `CiState = Pending | Success | Failure`.
- Origem: `commits(last: 1) { nodes { commit { statusCheckRollup { state contexts(first: 100) { ... } } } } }`.
  - `SUCCESS` vira `Success`. `FAILURE` e `ERROR` viram `Failure`. `PENDING` e `EXPECTED` viram `Pending`.
  - `failed_check`: nome do primeiro `CheckRun` com conclusão de falha, ou do primeiro `StatusContext` com estado de falha.
  - `finished_at`: maior `completedAt` dos `CheckRun`, só quando o estado não é `Pending`.
- `PENDING_QUERY` ganha esse trecho. `PendingPr` e `PrRecord` ganham `ci: Option<Ci>` (`#[serde(default)]`). O daemon atualiza o `ci` do registro a cada sync.
- **CI nunca derruba o sync de reviews.** Hoje qualquer campo ausente faz o repo virar `RepoSnapshot::Failed`, o que pausa a carência. O parse do CI é separado e tolerante: `statusCheckRollup` nulo ou ausente, estado desconhecido ou forma inesperada resultam em `ci: None`. `contexts` truncado mantém o `state` e pode deixar `failed_check` vazio.

### Meus PRs

- Query nova `MY_PRS_QUERY`, uma chamada por repo, com duas buscas com alias:
  - `open: search("repo:X is:pr is:open author:@me")`: `number title url isDraft createdAt` mais o mesmo trecho de CI. Paginado como o `fetch_pending`.
  - `recent: search("repo:X is:pr author:@me created:>=<agora−24h em ISO 8601>")`: só `issueCount`.
- Estado novo `State.my_prs: MyPrs` (`#[serde(default)]`):
  - `prs: BTreeMap<String, MyPrsRepo>` por repo, com `{ prs: Vec<MyPr>, opened_24h: u64, as_of }`
  - `errors: BTreeMap<String, String>`
  - `viewer: Option<String>`
- Roda no mesmo ciclo do poll. Passam a ser três chamadas ao `gh` por repo por ciclo.
- Falha num repo: mantém o último resultado bom dele e grava o erro. Erro de autenticação entra no `handle_auth` que já existe. Se o viewer mudar, os dados anteriores são descartados.
- Repo removido do config: os dados dele somem no ciclo seguinte.

## Painel

### Contadores

Um terceiro bloco, **Abertos 24h**, ao lado de **Pendentes** e **Feitas hoje**, com a soma de `opened_24h` dos repos. Se algum repo falhou, mostra o último valor e o erro aparece no rodapé.

### Seções e ordem

```
  PR           Estado                     Autor        Título
 Iniciados
▸ #15791     ● pronto · CI ✓ · 14:32 claude  fulano   Buyer own_setting…
  #15688     ● pronto · CI ◐ · 11:05 codex   beltrano Ajusta webhook…
 Pendentes
  #15802     ● pronto · CI ✗ rspec          ciclano  Novo filtro…
  #15799     ◐ preparando · CI ◐            fulano   Migra tags…
 Saindo
  #15650     ● pronto · sai em 9min         ...
 Adotados
  ...
 Meus PRs
  #15810     ◐ CI rodando                   você     Corrige import…
  #15790     ✓ CI 12min                     você     [draft] Refatora…
```

- `Section` passa a ser `Started, Pending, NoSync, Leaving, Adopted, Mine`, nesta ordem.
- **Iniciados**: registros `Managed` que estariam em `Pending` e têm `work_started_at`, ordenados pelo início mais recente.
- **Pendentes** ganha título, que antes não tinha.
- Um PR iniciado que sai da fila vai para **Saindo**, e não fica em Iniciados. A contagem regressiva da remoção não pode ficar escondida. O trecho `HH:MM agente` continua na linha em qualquer seção, inclusive em Adotados e Sem sync.
- **Meus PRs**: ordenados pelo `createdAt` mais recente. A coluna Autor mostra `você`.

### Texto do estado

- Linhas de review: o texto atual, mais ` · CI <ícone>[ <detalhe>]`, mais ` · HH:MM <agente>` quando iniciado.
- Ícones de CI: `◐` rodando, `✓ <tempo desde finished_at>`, `✗ <failed_check>`, `?` sem dado.
- Em Meus PRs o ícone da linha é o do CI, porque lá não existe phase.

### Teclas

- Em Meus PRs: `enter` e `o` abrem o PR no navegador. `l`, `R`, `a` e `r` não estão disponíveis (`Eligibility` toda falsa, exceto abrir).
- `first-ready` continua considerando só PRs de review.

### CLI

`herdr-reviewq status` ganha o CI e o início em cada PR de review, uma seção "Meus PRs" e a linha "Abertos nas últimas 24h: N".

## Testes

Seguem o padrão do repo: `Runner` falso e fixtures JSON.

- **CI:** os quatro estados do rollup, `ERROR` e `EXPECTED`, `failed_check` vindo de `CheckRun` e de `StatusContext`, `finished_at`, `contexts` truncado, rollup nulo. Um CI malformado não transforma o repo em `Failed`.
- **Meus PRs:** lista, draft, paginação, `issueCount`, erro GraphQL, falha parcial que mantém o resultado anterior do repo, troca de viewer.
- **Agentes:** `cwd` igual ao worktree, `cwd` em subdiretório, `foreground_cwd`, prefixo parecido que não casa, valor estável entre ciclos, falha do herdr sem efeito.
- **Modelo do TUI:** ordem das seções, iniciado que vai para Saindo, elegibilidade das teclas em Meus PRs, `first-ready` ignorando Meus PRs.
- **Render:** contador "Abertos 24h", título "Pendentes", largura mínima de 60 colunas.
- **State:** um `state.json` antigo, sem os campos novos, carrega sem erro.

## Fora de escopo

- Notificação de CI.
- Usar o início do trabalho para proteger o worktree da remoção.
- Worktree ou setup para os meus PRs.
- PRs fora dos repos do config.
- Review decision (aprovado, mudanças pedidas) nos meus PRs.
