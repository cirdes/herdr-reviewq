# Instruções para agentes

## PR com mudança de interface

Quando o PR muda o que o painel desenha (`src/tui/`, `src/ui.rs` ou qualquer texto que aparece na tela), a descrição traz uma prévia em texto de como a tela fica:

- Gere a tela com o `TestBackend` do ratatui, do mesmo jeito que a função `screen` dos testes de `src/tui/render.rs`: um teste temporário que monta um `State` com dados fictícios e imprime o resultado com `cargo test <nome> -- --nocapture`.
- Mostre pelo menos duas larguras: uma larga (cerca de 110 colunas) e uma estreita (60 a 72), porque as colunas somem e encolhem conforme a largura.
- Quando ajudar a entender a mudança, mostre também como era antes.
- Cole a saída em blocos de código e corte as linhas em branco do fim.
- Use só dados fictícios (repos, autores, títulos), nunca dados de PRs reais.
- Apague o teste temporário antes do commit.
