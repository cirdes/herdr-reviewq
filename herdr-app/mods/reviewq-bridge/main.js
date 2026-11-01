// Ponte do reviewq para uma janela em outra máquina. Quando o daemon do reviewq roda numa
// máquina conectada (ex.: macmini) e esta janela roda aqui, o Enter do painel não alcança o
// herdr-app: ele grava { space, at } no doc `reviewq-focus` do herdr-core de lá. Este mod lê
// esse doc em cada máquina conectada e mostra o space pedido.
import { View, div } from "gpui-kit";
import { h_flex } from "gpui-base";
import { exec, machines, run, spaces, theme } from "herdr";

const DOC = "reviewq-focus";
const EVERY_MS = 1000;
// Pedido mais velho que isso (feito com a janela fechada ou a máquina fora) não rouba o foco.
const FRESH_MS = 30000;

// O pedido vai como argumento do sh, nunca dentro do script.
async function docGet(machine) {
  const req = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "doc.get", params: { name: DOC, on: machine } });
  const r = await exec("sh", ["-c", 'printf "%s\\n" "$1" | hc connect', "sh", req]);
  if (r.code !== 0) throw new Error(r.stderr.trim() || `hc connect saiu com ${r.code}`);
  const res = JSON.parse(r.stdout);
  if (res.error) throw new Error(res.error.message);
  return res.result;
}

export default class ReviewqBridge extends View {
  init(_props, cx) {
    this.seen = {}; // máquina -> versão do doc já vista
    this.errors = {}; // máquina -> último erro
    this.last = null; // último foco feito: { id, at }
    this.busy = false;
    cx.timer.every(EVERY_MS, (cx) => this.poll(cx));
    this.poll(cx);
  }

  poll(cx) {
    if (this.busy) return;
    this.busy = true;
    cx.spawn(async (cx) => {
      let changed = false;
      for (const m of machines()) {
        if (m.name === null || m.layout !== "ready") continue;
        try {
          changed = (await this.check(m.name)) || changed;
          if (this.errors[m.name]) {
            delete this.errors[m.name];
            changed = true;
          }
        } catch (e) {
          const msg = String(e?.message ?? e);
          if (this.errors[m.name] !== msg) {
            this.errors[m.name] = msg;
            changed = true;
          }
        }
      }
      this.busy = false;
      // Redesenhar repinta a janela inteira: só quando algo mudou.
      if (changed) cx.notify();
    });
  }

  // Na primeira leitura de cada máquina só anota a versão: pedido antigo não vira foco.
  async check(machine) {
    const { value, version } = await docGet(machine);
    const first = !(machine in this.seen);
    if (!first && version === this.seen[machine]) return false;
    this.seen[machine] = version;
    if (first || !value?.space || Date.now() - (value.at ?? 0) > FRESH_MS) return first;
    const id = `${machine}/${value.space}`;
    run("space.select", { id });
    this.last = { id, at: Date.now() };
    return true;
  }

  render(_cx) {
    const t = theme();
    const remotes = machines().filter((m) => m.name !== null);
    const errors = Object.entries(this.errors);
    let text;
    if (errors.length) {
      text = errors.map(([m, e]) => `${m}: ${e}`).join(" · ");
    } else if (!remotes.length) {
      text = "nenhuma máquina conectada";
    } else if (this.last) {
      const name = spaces().find((s) => s.id === this.last.id)?.name ?? this.last.id;
      const hhmm = new Date(this.last.at).toTimeString().slice(0, 5);
      text = `${remotes.map((m) => m.name).join(", ")} · ${hhmm} ${name}`;
    } else {
      text = `ouvindo ${remotes.map((m) => m.name).join(", ")}`;
    }
    return h_flex().id("root").size_full().items_center().gap(6).px(10).bg(t.chrome).text_size(11)
      .child(div().text_color(t.muted).child("reviewq"))
      .child(div().flex_1().truncate().text_color(errors.length ? t.blocked : t.dim).child(text));
  }
}
