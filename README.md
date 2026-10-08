# herdr-reviewq

Daemon que monta worktrees no herdr para os PRs com review pedido diretamente a você e prepara o ambiente. Quando o PR deixa de estar pendente, remove o worktree só se ele estiver intocado e só depois da carência (`remove_grace_secs`, 15 min por padrão). Worktree adotado ou alterado fica onde está, com alerta no `status`. PRs de fork e pedidos feitos só ao time são ignorados.

## Instalar (macOS)

1. Pré-requisitos: `gh` autenticado (`gh auth login` + `gh auth setup-git`), `mise`, herdr 0.9.3+, Rust (cargo) e um clone local de cada repo que você revisa.
2. `mkdir -p ~/.config/herdr-reviewq && cp config.example.toml ~/.config/herdr-reviewq/config.toml` e ajustar `repos` (nome, caminho do clone, comandos de setup) e `disposable_ignored` (ignorados que o setup cria, como `node_modules`). Chave desconhecida na config é erro.
3. `cargo build --release && ./target/release/herdr-reviewq service install` (copia o binário para `~/.local/bin` e registra o LaunchAgent).

## Usar

- `herdr-reviewq status` — pendentes, prontos, adotados, alertas.
- `herdr-reviewq request sync` — consulta o GitHub agora.
- `herdr-reviewq request retry --pr owner/repo#123` — refaz setup que falhou ou recria worktree bloqueado (só para worktree gerenciado).
- `herdr-reviewq request adopt --pr ...` — marca o worktree como seu antes de mexer nele.
- `herdr-reviewq request release --pr ...` — libera um worktree adotado (só se estiver limpo e com tudo no origin).
- `herdr-reviewq service restart` — reinicia o daemon (depois de mudar a config); `service uninstall` — remove o LaunchAgent.
- Logs: `~/.local/state/herdr-reviewq/daemon.log` e `logs/` (um por PR, saída redigida).

## Painel no herdr

1. `cargo build --release && ./target/release/herdr-reviewq service install` (troca o binário estável em `~/.local/bin` e reinicia o daemon).
2. `herdr plugin link ~/Workspaces/herdr-reviewq` (uma vez; vale na hora, sem `reload-config`). O link aponta para o working tree do repo: um checkout de branch sem `herdr-plugin.toml` e `bin/herdr-reviewq` quebra o painel e as actions; ao voltar, rode `herdr plugin link` de novo.
3. O daemon cria o workspace `reviewq` ao subir. Para trazê-lo de volta: action "reviewq: abrir painel" (associe um atalho no herdr) ou `herdr-reviewq ui open`. A action "reviewq: primeiro PR pronto" (`herdr-reviewq focus first-ready`) leva ao PR pronto mais antigo, e "reviewq: sincronizar agora" equivale a `request sync`.
4. Atualizações: repetir o passo 1. O painel aberto continua com o binário antigo até ser reaberto: feche o pane e use "abrir painel".

Sair do TUI (`q`) fecha o pane, porque o pane termina junto com o comando. O plugin sempre executa o binário instalado em `~/.local/bin`. Início, saída (tecla ou sinal) e erros do TUI ficam em `~/.local/state/herdr-reviewq/logs/tui.log`.

### Teclas do painel

| Tecla | Ação |
|---|---|
| `↑` `↓` / `k` `j` | mover a seleção |
| `enter` | abrir o workspace do PR (recria se estiver fechado) |
| `o` | abre o PR no navegador da máquina do daemon e copia a URL via OSC 52 (útil em sessão remota) |
| `l` | abrir o log do PR |
| `s` | sincronizar agora |
| `R` | tentar de novo (só PR gerenciado com setup falho ou bloqueado sem worktree) |
| `a` | adotar o worktree (pede confirmação `y`/`n`) |
| `r` | liberar o worktree adotado (pede confirmação `y`/`n`) |
| `q` / `esc` / `ctrl+c` | sair |

## Garantias

- Remove sozinho só depois da carência e só worktree impecável: mesma branch e sha que o daemon colocou, nada alterado, nada novo, nenhum ignorado fora da lista de descartáveis.
- Nunca roda `reset --hard`, `clean` nem `--force`; atualização usa `reset --keep`.
- Branch só é apagada se foi o daemon que a criou, se ainda está no sha dele e se não está aberta em outro worktree.
- Antes de reset ou remoção, o sha anterior fica em `refs/reviewq/backup/pr-<n>/<ts>` por 14 dias.
- Falha ao consultar o GitHub nunca remove nada; falha ao salvar o estado interrompe o ciclo.
