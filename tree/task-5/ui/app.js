"use strict";

const invoke = window.__TAURI__?.core?.invoke;

const TONES = { weak: "var(--weak)", medium: "var(--medium)", strong: "var(--strong)" };

const PRESETS = [
  ["Техника", "Объясни разницу между HTTP-кэшированием по ETag и по Last-Modified: когда какой выбрать и какие ошибки чаще всего допускают. До 200 слов."],
  ["Разбор", "Почему транзакция в базе может пройти успешно, но данные всё равно потеряются? Разбери по шагам."],
  ["Код", "Напиши функцию на Python, которая находит цикл в односвязном списке за O(1) памяти. Только код и короткий комментарий."],
  ["Творчество", "Придумай название и слоган для сервиса, который сравнивает языковые модели по цене."],
];

const VERDICTS = {
  confirmed: ["ok", "лестница подтверждена"],
  substituted: ["bad", "площадка ответила другой моделью"],
  flat: ["meh", "проба не различила ступени"],
  inverted: ["meh", "порядок ступеней не подтверждён"],
};

const $ = (id) => document.getElementById(id);
let lastLadder = null;
/** Проверка не зависит от запроса — гоняем её один раз за сессию. */
let checked = false;

/* ---------- разметка ---------- */

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

/** Крошечный markdown: заголовки, списки, **жирный**, `код`. Больше не нужно. */
function renderMarkdown(src) {
  const inline = (t) =>
    escapeHtml(t)
      .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
      .replace(/`(.+?)`/g, "<code>$1</code>");

  const out = [];
  let list = null;
  const closeList = () => {
    if (list) {
      out.push(`<ul>${list.join("")}</ul>`);
      list = null;
    }
  };

  for (const raw of src.split("\n")) {
    const line = raw.trim();
    if (!line) { closeList(); continue; }
    const heading = line.match(/^#{1,6}\s+(.*)$/);
    const item = line.match(/^[-*]\s+(.*)$/);
    if (heading) {
      closeList();
      out.push(`<h2>${inline(heading[1])}</h2>`);
    } else if (item) {
      (list ??= []).push(`<li>${inline(item[1])}</li>`);
    } else {
      closeList();
      out.push(`<p>${inline(line)}</p>`);
    }
  }
  closeList();
  return out.join("");
}

/** Шкала всегда относительно худшей ступени: важно не значение, а во сколько раз. */
function meter(label, value, fraction, tone) {
  const pct = Math.max(0, Math.min(1, fraction)) * 100;
  return `<div>
    <div class="meter-label"><span>${label}</span><b>${escapeHtml(value)}</b></div>
    <div class="bar"><i style="width:${pct.toFixed(1)}%;background:${tone}"></i></div>
  </div>`;
}

function money(usd) {
  return usd >= 0.01 ? `$${usd.toFixed(3)}` : `$${usd.toFixed(5)}`;
}

function cardHtml(rung, max) {
  const tone = TONES[rung.tier] ?? "var(--accent)";
  const m = rung.measure;
  const cost = rung.billing === "subscription"
    ? `подписка · по прайсу ${money(m.cost_usd)}`
    : money(m.cost_usd);

  const meters = [
    meter("время ответа", `${(m.latency_ms / 1000).toFixed(1)} с`, m.latency_ms / max.latency_ms, tone),
    meter("токенов на ответ", `${m.completion_tokens}`, m.completion_tokens / max.completion_tokens, tone),
    meter("стоимость", cost, m.cost_usd / max.cost_usd, tone),
    meter("скорость", `${m.tokens_per_sec.toFixed(0)} ток./с`, m.tokens_per_sec / max.tokens_per_sec, tone),
  ].join("");

  return `<article class="card tcard" style="--tone:${tone}">
    <h3>${escapeHtml(rung.label)}<b>${rung.score === null ? "—" : rung.score + "/10"}</b></h3>
    <div class="model-line">
      <a href="${escapeHtml(rung.model_url)}" target="_blank" rel="noreferrer">${escapeHtml(rung.model)}</a>
      ${rung.effort ? `<span class="fact">${escapeHtml(rung.effort)}</span>` : ""}
    </div>
    <div class="facts">
      <span class="fact">${escapeHtml(rung.provider)}</span>
      <span class="fact">${m.reasoning_tokens} ток. рассуждений</span>
      <span class="fact">${escapeHtml(rung.finish_reason)}</span>
    </div>
    <div class="answer">${escapeHtml(rung.answer)}</div>
    <div class="meters">${meters}</div>
  </article>`;
}

/* ---------- проверка лестницы ---------- */

/** Без неё замеры не значат ничего: неизвестно даже, та ли это модель. */
function renderCheck(c) {
  const [tone, label] = VERDICTS[c.verdict] ?? ["meh", c.verdict];
  const verdict = $("verdict");
  verdict.className = `pill verdict ${tone}`;
  verdict.textContent = label;
  verdict.classList.remove("hidden");

  const rows = c.rungs
    .map(
      (r) => `<tr>
        <td>${escapeHtml(r.label)}</td>
        <td>${r.model_matches ? "✅" : "❌"} <code>${escapeHtml(r.served_model)}</code></td>
        <td><b>${r.score}/${c.questions.length}</b></td>
        <td>${r.answers
          .map((a, i) => `<span class="${r.correct[i] ? "ok" : "bad"}">${escapeHtml(a.split("\n")[0].slice(0, 24))}</span>`)
          .join(" ")}</td>
      </tr>`
    )
    .join("");

  const el = $("check");
  el.className = `check ${tone}`;
  el.innerHTML = `
    <div><b>${escapeHtml(label)}</b> — ${c.questions.length} вопроса с известным ответом при temperature = 0</div>
    <table class="probe-table"><tbody>${rows}</tbody></table>
    <div class="muted">${escapeHtml(c.explanation)}</div>`;
  el.classList.remove("hidden");
}

async function verifyLadder() {
  const btn = $("verify");
  btn.disabled = true;
  setStatus("Проба: по 4 коротких вопроса на каждую из трёх ступеней", false);
  try {
    renderCheck(await invoke("check_ladder"));
    checked = true;
    $("status").classList.add("hidden");
  } catch (e) {
    setStatus(String(e), true);
  } finally {
    btn.disabled = false;
  }
}

/* ---------- поток ---------- */

function setStatus(text, isError) {
  const el = $("status");
  el.className = "status" + (isError ? " err" : "");
  el.innerHTML = isError ? escapeHtml(text) : `${escapeHtml(text)}<span class="dots"></span>`;
  el.classList.remove("hidden");
}

function render(l) {
  lastLadder = l;
  // Шкалы метрик общие на все карточки, иначе «в 20 раз быстрее» не видно.
  const max = {
    latency_ms: Math.max(...l.rungs.map((r) => r.measure.latency_ms), 1),
    completion_tokens: Math.max(...l.rungs.map((r) => r.measure.completion_tokens), 1),
    cost_usd: Math.max(...l.rungs.map((r) => r.measure.cost_usd), 1e-9),
    tokens_per_sec: Math.max(...l.rungs.map((r) => r.measure.tokens_per_sec), 1),
  };
  $("cards").innerHTML = l.rungs.map((r) => cardHtml(r, max)).join("");
  $("judge-name").textContent = l.judge_model;
  const total = l.rungs.reduce((s, r) => s + r.measure.cost_usd, 0);
  $("timing").textContent = `${l.rungs.length + 1} запрос(ов) · ${(l.total_ms / 1000).toFixed(1)} с · ${money(total)}`;
  $("summary").innerHTML = renderMarkdown(l.summary);
  $("results").classList.remove("hidden");
  if (l.check) renderCheck(l.check);
}

async function compare() {
  const prompt = $("prompt").value.trim();
  if (!prompt) {
    setStatus("Сначала введите запрос.", true);
    return;
  }
  if (!invoke) {
    setStatus("Нет моста Tauri — откройте приложение, а не файл в браузере.", true);
    return;
  }

  const verify = !checked;
  $("go").disabled = true;
  $("results").classList.add("hidden");
  setStatus(
    "Один запрос уходит на три модели параллельно, затем разбор судьёй" +
      (verify ? ", плюс проверка лестницы" : ""),
    false
  );

  try {
    render(await invoke("run_ladder", { prompt, verify }));
    checked = true;
    $("status").classList.add("hidden");
  } catch (e) {
    setStatus(String(e), true);
  } finally {
    $("go").disabled = false;
  }
}

async function copyReport() {
  if (!lastLadder || !invoke) return;
  const md = await invoke("markdown_report", { ladder: lastLadder });
  await navigator.clipboard.writeText(md);
  const btn = $("copy");
  btn.textContent = "Скопировано";
  setTimeout(() => (btn.textContent = "Скопировать отчёт"), 1500);
}

/* ---------- запуск ---------- */

$("presets").innerHTML = PRESETS.map(
  ([label], i) => `<button data-i="${i}">${label}</button>`
).join("");
$("presets").addEventListener("click", (e) => {
  const i = e.target.dataset?.i;
  if (i !== undefined) $("prompt").value = PRESETS[i][1];
});

$("go").addEventListener("click", compare);
$("verify").addEventListener("click", verifyLadder);
$("copy").addEventListener("click", copyReport);
$("prompt").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) compare();
});

$("prompt").value = PRESETS[0][1];

(async () => {
  if (!invoke) {
    setStatus("Нет моста Tauri — откройте приложение, а не файл в браузере.", true);
    return;
  }
  const list = await invoke("tiers");
  $("tier-pills").insertAdjacentHTML(
    "afterbegin",
    list
      .map(
        (t) =>
          `<span class="pill" style="--tone:${TONES[t.tier]}">${escapeHtml(t.label)}: <b>${escapeHtml(
            t.model
          )}</b>${t.effort ? ` · ${escapeHtml(t.effort)}` : ""}</span>`
      )
      .join("")
  );
})();
