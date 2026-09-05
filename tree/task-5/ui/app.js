"use strict";

const invoke = window.__TAURI__?.core?.invoke;

const TONES = { 0: "var(--t0)", 0.7: "var(--t07)", 1.2: "var(--t12)" };
const HINTS = {
  0: "жёсткий детерминизм",
  0.7: "рабочий баланс",
  1.2: "широкий сэмплинг",
};

const PRESETS = [
  ["Факты", "Что такое temperature в языковой модели? Ответь в 3 предложениях."],
  ["Творчество", "Придумай название и слоган для кофейни на берегу моря."],
  ["Код", "Напиши функцию на Python, которая разворачивает односвязный список. Только код."],
  ["Идеи", "Дай 5 идей, чем занять ребёнка 7 лет в дождливый день."],
];

const VERDICTS = {
  honored: ["ok", "temperature применяется"],
  ignored: ["bad", "temperature игнорируется"],
  inconclusive: ["meh", "проверка не показательна"],
};

const $ = (id) => document.getElementById(id);
let lastComparison = null;
/** Провайдеры, для которых проба уже прогонялась в этой сессии. */
const checked = new Set();

/* ---------- разметка ---------- */

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) =>
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

function meter(label, value, fraction, tone) {
  const pct = Math.max(0, Math.min(1, fraction)) * 100;
  return `<div>
    <div class="meter-label"><span>${label}</span><b>${value}</b></div>
    <div class="bar"><i style="width:${pct.toFixed(1)}%;background:${tone}"></i></div>
  </div>`;
}

function cardHtml(branch, maxWords) {
  const t = branch.temperature;
  const tone = TONES[t] ?? "var(--accent)";
  const m = branch.metrics;

  // «Разнообразие» — обратная величина совпадения повторных прогонов.
  const diversity =
    m.self_similarity === null || m.self_similarity === undefined
      ? null
      : 1 - m.self_similarity;

  const meters = [
    meter("лексическое богатство", `${Math.round(m.distinct_ratio * 100)}%`, m.distinct_ratio, tone),
    meter("объём", `${m.words} слов`, maxWords ? m.words / maxWords : 0, tone),
    diversity === null
      ? ""
      : meter("разнообразие прогонов", `${Math.round(diversity * 100)}%`, diversity, tone),
  ].join("");

  const extras = branch.runs.slice(1);
  const extrasHtml = extras.length
    ? `<details class="extra">
         <summary>ещё ${extras.length} прогон(а) при этой же температуре</summary>
         ${extras
           .map(
             (r, i) =>
               `<div class="run-tag">прогон ${i + 2}</div><div class="answer">${escapeHtml(r.content)}</div>`
           )
           .join("")}
       </details>`
    : "";

  const first = branch.runs[0];
  return `<article class="card tcard" style="--tone:${tone}">
    <h3>temperature<b>${t}</b></h3>
    <div class="facts">
      <span class="fact">${HINTS[t] ?? ""}</span>
      <span class="fact">${first.completion_tokens} ток.</span>
      <span class="fact">${(first.latency_ms / 1000).toFixed(1)} с</span>
      <span class="fact">${first.finish_reason}</span>
    </div>
    <div class="answer">${escapeHtml(branch.answer)}</div>
    <div class="meters">${meters}</div>
    ${extrasHtml}
  </article>`;
}

/* ---------- проверка температуры ---------- */

/** Показывает результат пробы: без него «различия» между ветками ничего не значат. */
function renderCheck(c) {
  const [tone, label] = VERDICTS[c.verdict] ?? ["meh", c.verdict];
  const verdict = $("verdict");
  verdict.className = `pill verdict ${tone}`;
  verdict.textContent = label;
  verdict.classList.remove("hidden");

  const el = $("check");
  el.className = `check ${tone}`;
  el.innerHTML = `
    <div><b>${escapeHtml(label)}</b> — <code>${escapeHtml(c.provider)}</code> / <code>${escapeHtml(c.model)}</code></div>
    <div class="probe">
      <span>temperature=${c.cold_temperature} → <b>${escapeHtml(c.cold_answers.join(" "))}</b> (${c.cold_distinct} разных)</span>
      <span>temperature=${c.hot_temperature} → <b>${escapeHtml(c.hot_answers.join(" "))}</b> (${c.hot_distinct} разных)</span>
    </div>
    <div class="muted">${escapeHtml(c.explanation)}</div>`;
  el.classList.remove("hidden");
}

async function verifyTemperature() {
  const btn = $("verify");
  btn.disabled = true;
  setStatus("Проба: по 6 коротких запросов при temperature 0 и 2.0", false);
  try {
    renderCheck(await invoke("check_temperature", { provider: $("provider").value }));
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

function render(c) {
  lastComparison = c;
  const maxWords = Math.max(...c.branches.map((b) => b.metrics.words), 1);
  $("cards").innerHTML = c.branches.map((b) => cardHtml(b, maxWords)).join("");
  $("judge-name").textContent = c.judge_model;
  $("worker-model").textContent = c.worker_model;
  $("judge-model").textContent = c.judge_model;
  $("timing").textContent = `${c.branches.length * c.runs_per_temperature + 1} запрос(ов) · ${(
    c.total_ms / 1000
  ).toFixed(1)} с`;
  $("summary").innerHTML = renderMarkdown(c.summary);
  $("results").classList.remove("hidden");
  if (c.temp_check) renderCheck(c.temp_check);
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

  const provider = $("provider").value;
  const runs = Number($("runs").value);
  // Проверку гоняем один раз на провайдера: она не зависит от запроса.
  const verify = !checked.has(provider);
  $("go").disabled = true;
  $("results").classList.add("hidden");
  setStatus(
    `Идут ${3 * runs + 1} запроса: три температуры по ${runs} прогон(а), затем разбор` +
      (verify ? ", плюс проверка температуры" : ""),
    false
  );

  try {
    render(await invoke("compare_temperatures", { prompt, runs, provider, verify }));
    checked.add(provider);
    $("status").classList.add("hidden");
  } catch (e) {
    setStatus(String(e), true);
  } finally {
    $("go").disabled = false;
  }
}

async function copyReport() {
  if (!lastComparison || !invoke) return;
  const md = await invoke("markdown_report", { comparison: lastComparison });
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
$("verify").addEventListener("click", verifyTemperature);
$("copy").addEventListener("click", copyReport);
$("prompt").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) compare();
});

// Смена провайдера обесценивает и вердикт, и показанные ответы.
$("provider").addEventListener("change", () => {
  $("check").classList.add("hidden");
  $("verdict").classList.add("hidden");
  $("results").classList.add("hidden");
  $("worker-model").textContent = $("provider").selectedOptions[0].dataset.model;
});

$("prompt").value = PRESETS[1][1];

(async () => {
  if (!invoke) {
    setStatus("Нет моста Tauri — откройте приложение, а не файл в браузере.", true);
    return;
  }
  const list = await invoke("providers");
  $("provider").innerHTML = list
    .map((p) => `<option value="${p.id}" data-model="${p.answer_model}">${p.id}</option>`)
    .join("");
  $("worker-model").textContent = list[0].answer_model;
})();
