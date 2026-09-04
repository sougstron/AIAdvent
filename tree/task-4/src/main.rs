// Без консольного окна на Windows; на Linux атрибут игнорируется.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod compare;
mod report;
mod verify;

use std::process::ExitCode;

use api::{Client, Provider};

/// Кнопка «Сравнить»: три температуры, затем разбор старшей моделью.
#[tauri::command]
async fn compare_temperatures(
    prompt: String,
    runs: usize,
    provider: String,
    verify: bool,
) -> Result<compare::Comparison, String> {
    // Сеть блокирующая (ureq), поэтому уводим её с асинхронного рантайма.
    tauri::async_runtime::spawn_blocking(move || {
        let client = Client::new(Provider::parse(&provider)?)?;
        let mut comparison = compare::run(&client, &prompt, runs)?;
        if verify {
            comparison.temp_check = Some(verify::check_temperature(&client)?);
        }
        Ok(comparison)
    })
    .await
    .map_err(|e| format!("задача не завершилась: {e}"))?
}

/// Кнопка «Проверить температуру» — отдельно от сравнения, чтобы можно было
/// сначала убедиться, что рычаг вообще работает.
#[tauri::command]
async fn check_temperature(provider: String) -> Result<verify::TempCheck, String> {
    tauri::async_runtime::spawn_blocking(move || {
        verify::check_temperature(&Client::new(Provider::parse(&provider)?)?)
    })
    .await
    .map_err(|e| format!("задача не завершилась: {e}"))?
}

/// Список провайдеров с моделями — фронт не должен их хардкодить.
#[tauri::command]
fn providers() -> Vec<serde_json::Value> {
    Provider::ALL
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id(),
                "answer_model": p.answer_model(),
            })
        })
        .collect()
}

/// Кнопка «Скопировать отчёт».
#[tauri::command]
fn markdown_report(comparison: compare::Comparison) -> String {
    report::to_markdown(&comparison)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help") | Some("-h") => {
            print_help();
            return ExitCode::SUCCESS;
        }
        Some("--cli") => return cli(&args[1..]),
        Some("--verify-temp") => return verify_temp(&args[1..]),
        _ => {}
    }

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            compare_temperatures,
            check_temperature,
            providers,
            markdown_report
        ])
        .run(tauri::generate_context!())
        .expect("не удалось запустить окно Tauri");
    ExitCode::SUCCESS
}

fn print_help() {
    println!(
        "Temperature Lab — один запрос при temperature {:?}\n\n\
         ask                                     окно приложения (Tauri)\n\
         ask --cli \"запрос\" [опции]               то же самое в терминале\n\
         ask --verify-temp [-p провайдер]        доказать, применяется ли temperature\n\n\
         Опции --cli:\n\
         \x20 -n, --runs N        прогонов на температуру (по умолчанию 1)\n\
         \x20 -p, --provider P    openrouter (по умолчанию) | zai\n\
         \x20 -o, --out ФАЙЛ      записать markdown-отчёт вместо вывода в stdout\n\
         \x20 --verify            добавить в отчёт проверку температуры\n\n\
         Ответы даёт выбранный провайдер, разбор — всегда {} ({}).\n\
         Ключи берутся из ~/.pi/agent/auth.json.\n",
        compare::TEMPERATURES,
        api::JUDGE_MODEL,
        api::JUDGE_PROVIDER.id()
    );
}

/// Провайдер по умолчанию для ответов. Не Z.AI: подписочный endpoint
/// игнорирует temperature (доказывается через `--verify-temp`), а на нём
/// сравнение температур бессмысленно.
const DEFAULT_PROVIDER: Provider = Provider::OpenRouter;

fn parse_provider(args: &[String], i: &mut usize) -> Result<Provider, String> {
    *i += 1;
    args.get(*i)
        .ok_or_else(|| "-p ждёт имя провайдера".to_string())
        .and_then(|v| Provider::parse(v))
}

fn verify_temp(args: &[String]) -> ExitCode {
    let mut provider = DEFAULT_PROVIDER;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" | "--provider" => match parse_provider(args, &mut i) {
                Ok(p) => provider = p,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            },
            other => {
                eprintln!("лишний аргумент: {other}");
                return ExitCode::FAILURE;
            }
        }
        i += 1;
    }

    let client = match Client::new(provider) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("нет доступа к API: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "проверяю {} / {}: по 6 прогонов при temperature 0 и 2.0…",
        provider.id(),
        provider.answer_model()
    );
    match verify::check_temperature(&client) {
        Ok(c) => {
            print!("{}", verify::to_text(&c));
            // Ненулевой код, если рычаг не подтверждён: удобно в скриптах.
            match c.verdict {
                verify::Verdict::Honored => ExitCode::SUCCESS,
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
    let mut runs = 1usize;
    let mut out_path: Option<String> = None;
    let mut provider = DEFAULT_PROVIDER;
    let mut with_check = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" | "--runs" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) => runs = n,
                    None => {
                        eprintln!("-n ждёт число");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "-p" | "--provider" => match parse_provider(args, &mut i) {
                Ok(p) => provider = p,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            },
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

    let client = match Client::new(provider) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("нет доступа к API: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "→ {} / {} × {} прогон(а) при temperature {:?}, затем разбор через {}…",
        provider.id(),
        provider.answer_model(),
        runs,
        compare::TEMPERATURES,
        api::JUDGE_MODEL
    );

    let mut comparison = match compare::run(&client, &prompt, runs) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("сравнение не удалось: {e}");
            return ExitCode::FAILURE;
        }
    };

    if with_check {
        eprintln!("→ проверяю, доезжает ли temperature до сэмплера…");
        match verify::check_temperature(&client) {
            Ok(c) => comparison.temp_check = Some(c),
            Err(e) => eprintln!("проверка температуры не удалась: {e}"),
        }
    }

    let md = report::to_markdown(&comparison);
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
