// Без консольного окна на Windows; на Linux атрибут игнорируется.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod ladder;
mod report;
mod verify;

use std::process::ExitCode;

/// WebKitGTK на некоторых Wayland/GPU-конфигурациях открывает пустое окно.
/// Применяем тот же workaround, что использует эталонный balance-editor, чтобы
/// релиз запускался обычным двойным кликом без обёрток и переменных окружения.
fn apply_linux_display_workarounds() {
    #[cfg(target_os = "linux")]
    {
        let native_wayland = std::env::var_os("MODEL_LADDER_NATIVE_WAYLAND").is_some();
        let on_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var("XDG_SESSION_TYPE").ok().as_deref() == Some("wayland");

        if on_wayland && !native_wayland && std::env::var_os("GDK_BACKEND").is_none() {
            std::env::set_var("GDK_BACKEND", "x11");
        }
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
        if std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_none() {
            std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        }
    }
}

/// Кнопка «Сравнить»: один запрос на три ступени, затем разбор судьёй.
#[tauri::command]
async fn run_ladder(prompt: String, verify: bool) -> Result<ladder::Ladder, String> {
    // Сеть блокирующая (ureq), поэтому уводим её с асинхронного рантайма.
    tauri::async_runtime::spawn_blocking(move || {
        let mut l = ladder::run(&prompt)?;
        if verify {
            l.check = Some(verify::check_ladder()?);
        }
        Ok(l)
    })
    .await
    .map_err(|e| format!("задача не завершилась: {e}"))?
}

/// Кнопка «Проверить лестницу» — отдельно от сравнения, чтобы можно было
/// сначала убедиться, что ступени вообще различаются.
#[tauri::command]
async fn check_ladder() -> Result<verify::LadderCheck, String> {
    tauri::async_runtime::spawn_blocking(verify::check_ladder)
        .await
        .map_err(|e| format!("задача не завершилась: {e}"))?
}

/// Состав лестницы — фронт не должен его хардкодить.
#[tauri::command]
fn tiers() -> Vec<serde_json::Value> {
    ladder::LADDER
        .iter()
        .map(|s| {
            serde_json::json!({
                "tier": s.tier,
                "label": s.label,
                "provider": s.provider.id(),
                "model": s.model,
                "effort": s.effort.map(|e| e.as_str()),
                "model_url": s.model_url,
                "price_url": s.price_url,
            })
        })
        .collect()
}

/// Кнопка «Скопировать отчёт».
#[tauri::command]
fn markdown_report(ladder: ladder::Ladder) -> String {
    report::to_markdown(&ladder)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help") | Some("-h") => {
            print_help();
            return ExitCode::SUCCESS;
        }
        Some("--cli") => return cli(&args[1..]),
        Some("--verify-ladder") => return verify_ladder(),
        _ => {}
    }

    apply_linux_display_workarounds();
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            run_ladder,
            check_ladder,
            tiers,
            markdown_report
        ])
        .run(tauri::generate_context!())
        .expect("не удалось запустить окно Tauri");
    ExitCode::SUCCESS
}

fn print_help() {
    println!(
        "Model Ladder — один запрос на слабой, средней и сильной модели\n\n\
         ask                             окно приложения (Tauri)\n\
         ask --cli \"запрос\" [опции]       то же самое в терминале\n\
         ask --verify-ladder             доказать, что ступени различимы\n\n\
         Опции --cli:\n\
         \x20 -o, --out ФАЙЛ    записать markdown-отчёт вместо вывода в stdout\n\
         \x20 --verify          добавить в отчёт проверку лестницы\n\n\
         Ступени:\n{}\
         \x20 судья    {} ({})\n\n\
         Ключи берутся из ~/.pi/agent/{{auth,models}}.json и \
         ~/.local/share/opencode/auth.json.\n",
        ladder::LADDER
            .iter()
            .map(|s| format!(
                "\x20 {:<8} {} ({}{})\n",
                s.label,
                s.model,
                s.provider.id(),
                match s.effort {
                    Some(e) => format!(", {}", e.as_str()),
                    None => String::new(),
                }
            ))
            .collect::<String>(),
        ladder::JUDGE_MODEL,
        ladder::JUDGE_PROVIDER.id()
    );
}

fn verify_ladder() -> ExitCode {
    eprintln!(
        "проверяю {} ступени: по {} коротких вопроса с известным ответом при temperature=0…",
        ladder::LADDER.len(),
        4
    );
    match verify::check_ladder() {
        Ok(c) => {
            print!("{}", verify::to_text(&c));
            // Ненулевой код, если лестница не подтверждена: удобно в скриптах.
            match c.verdict {
                verify::Verdict::Confirmed => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
        Err(e) => {
            eprintln!("проверка не удалась: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cli(args: &[String]) -> ExitCode {
    let mut prompt = String::new();
    let mut out_path: Option<String> = None;
    let mut with_check = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--out" => {
                i += 1;
                match args.get(i) {
                    Some(p) => out_path = Some(p.clone()),
                    None => {
                        eprintln!("-o ждёт путь");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--verify" => with_check = true,
            other => {
                if prompt.is_empty() {
                    prompt = other.to_string();
                } else {
                    eprintln!("лишний аргумент: {other}");
                    return ExitCode::FAILURE;
                }
            }
        }
        i += 1;
    }

    if prompt.is_empty() {
        eprintln!("нужен запрос: ask --cli \"...\"");
        return ExitCode::FAILURE;
    }

    eprintln!(
        "→ один запрос на {} ступени параллельно, затем разбор через {}…",
        ladder::LADDER.len(),
        ladder::JUDGE_MODEL
    );

    let mut l = match ladder::run(&prompt) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("сравнение не удалось: {e}");
            return ExitCode::FAILURE;
        }
    };

    if with_check {
        eprintln!("→ проверяю, различимы ли ступени…");
        match verify::check_ladder() {
            Ok(c) => l.check = Some(c),
            Err(e) => eprintln!("проверка лестницы не удалась: {e}"),
        }
    }

    let md = report::to_markdown(&l);
    match out_path {
        Some(p) => match std::fs::write(&p, &md) {
            Ok(()) => eprintln!("отчёт записан: {p}"),
            Err(e) => {
                eprintln!("не записать {p}: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => print!("{md}"),
    }
    ExitCode::SUCCESS
}
