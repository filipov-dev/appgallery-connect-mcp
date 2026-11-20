//! Взаимодействие с пользователем в интерактивных командах (`auth`, `connect`).
//!
//! Вынесено в трейт, чтобы сценарии можно было тестировать без терминала.

use std::io::{self, BufRead, IsTerminal, Write};

use zeroize::Zeroizing;

pub trait Ui {
    /// Выводит строку пользователю.
    fn say(&mut self, line: &str);
    /// Обычный (видимый) ввод строки, например идентификатор API-клиента.
    fn ask(&mut self, prompt: &str) -> io::Result<String>;
    /// Скрытый ввод секрета: значение не отображается в терминале.
    fn ask_secret(&mut self, prompt: &str) -> io::Result<Zeroizing<String>>;
    /// Вопрос «да/нет»; пустой ответ — `default`. EOF на stdin считается отказом.
    fn confirm(&mut self, question: &str, default: bool) -> io::Result<bool>;
    /// Можно ли задавать вопросы: stdin — терминал. В CI и скриптах вопросов нет.
    fn is_interactive(&self) -> bool;
    /// Открывает ссылку в браузере по умолчанию (вход пользователя).
    fn open_url(&mut self, url: &str) -> io::Result<()>;
}

/// Разбирает ответ на вопрос «да/нет». `None` — ответ не распознан.
pub fn parse_yes_no(answer: &str, default: bool) -> Option<bool> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => Some(default),
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// Терминал: вывод в stdout, ввод из stdin, секрет — через `rpassword` (читает напрямую
/// из TTY без эха).
#[derive(Debug, Default)]
pub struct TerminalUi;

impl Ui for TerminalUi {
    fn say(&mut self, line: &str) {
        println!("{line}");
    }

    fn ask(&mut self, prompt: &str) -> io::Result<String> {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut answer = String::new();
        if io::stdin().lock().read_line(&mut answer)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "no answer on stdin",
            ));
        }
        Ok(answer.trim().to_owned())
    }

    fn ask_secret(&mut self, prompt: &str) -> io::Result<Zeroizing<String>> {
        rpassword::prompt_password(prompt).map(Zeroizing::new)
    }

    fn confirm(&mut self, question: &str, default: bool) -> io::Result<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            print!("{question} {hint} ");
            io::stdout().flush()?;
            let mut answer = String::new();
            // EOF (например, stdin не терминал) считается отказом, а не ответом по умолчанию.
            if io::stdin().lock().read_line(&mut answer)? == 0 {
                println!();
                return Ok(false);
            }
            match parse_yes_no(&answer, default) {
                Some(value) => return Ok(value),
                None => println!("Please answer y or n."),
            }
        }
    }

    fn is_interactive(&self) -> bool {
        io::stdin().is_terminal()
    }

    fn open_url(&mut self, url: &str) -> io::Result<()> {
        crate::browser::open_in_browser(url)
    }
}

/// Тихий режим (`connect --silent`): ничего не выводит и ничего не спрашивает. Команда ведёт
/// себя как без терминала, ошибки печатает `main` в stderr.
#[derive(Debug, Default)]
pub struct SilentUi;

impl SilentUi {
    fn no_questions() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "--silent: no questions are asked",
        )
    }
}

impl Ui for SilentUi {
    fn say(&mut self, _line: &str) {}

    fn ask(&mut self, _prompt: &str) -> io::Result<String> {
        Err(Self::no_questions())
    }

    fn ask_secret(&mut self, _prompt: &str) -> io::Result<Zeroizing<String>> {
        Err(Self::no_questions())
    }

    fn confirm(&mut self, _question: &str, _default: bool) -> io::Result<bool> {
        Err(Self::no_questions())
    }

    fn is_interactive(&self) -> bool {
        false
    }

    fn open_url(&mut self, _url: &str) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "--silent: the browser is not opened",
        ))
    }
}

#[cfg(test)]
pub mod testing {
    use std::collections::VecDeque;

    use super::*;

    /// Сценарий ввода для тестов: заранее заданные ответы, весь вывод сохраняется.
    #[derive(Debug, Default)]
    pub struct ScriptedUi {
        /// Ответы на видимые вопросы и на вопросы «да/нет» (`y`, `n`, пусто), по порядку.
        pub answers: VecDeque<String>,
        /// Ответы на скрытые вопросы (секреты), по порядку.
        pub secrets: VecDeque<String>,
        pub output: Vec<String>,
        pub questions: Vec<String>,
        /// Сценарий изображает терминал: можно задавать вопросы.
        pub interactive: bool,
        /// Ссылки, которые открыл бы браузер.
        pub opened: Vec<String>,
        /// Браузер не открывается.
        pub open_fails: bool,
    }

    impl ScriptedUi {
        /// Интерактивный сценарий с видимыми ответами `answers` и секретами `secrets`.
        pub fn answering(answers: &[&str], secrets: &[&str]) -> Self {
            Self {
                answers: answers.iter().map(|a| (*a).to_owned()).collect(),
                secrets: secrets.iter().map(|a| (*a).to_owned()).collect(),
                interactive: true,
                ..Self::default()
            }
        }

        pub fn text(&self) -> String {
            self.output.join("\n")
        }
    }

    impl Ui for ScriptedUi {
        fn say(&mut self, line: &str) {
            self.output.push(line.to_owned());
        }

        fn ask(&mut self, prompt: &str) -> io::Result<String> {
            self.questions.push(prompt.to_owned());
            self.answers
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no answer scripted"))
        }

        fn ask_secret(&mut self, prompt: &str) -> io::Result<Zeroizing<String>> {
            self.questions.push(prompt.to_owned());
            self.secrets
                .pop_front()
                .map(Zeroizing::new)
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no secret scripted"))
        }

        fn confirm(&mut self, question: &str, default: bool) -> io::Result<bool> {
            let answer = self.ask(question)?;
            parse_yes_no(&answer, default).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unexpected yes/no answer")
            })
        }

        fn is_interactive(&self) -> bool {
            self.interactive
        }

        fn open_url(&mut self, url: &str) -> io::Result<()> {
            if self.open_fails {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no browser"));
            }
            self.opened.push(url.to_owned());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_ui_says_nothing_and_asks_nothing() {
        let mut ui = SilentUi;
        ui.say("hello");
        assert!(!ui.is_interactive());
        assert!(ui.ask("id?").is_err());
        assert!(ui.ask_secret("secret?").is_err());
        assert!(ui.confirm("ok?", true).is_err());
        assert!(ui.open_url("https://example.com").is_err());
    }

    #[test]
    fn yes_no_answers() {
        assert_eq!(parse_yes_no("", true), Some(true));
        assert_eq!(parse_yes_no(" ", false), Some(false));
        assert_eq!(parse_yes_no("Y", false), Some(true));
        assert_eq!(parse_yes_no("yes", false), Some(true));
        assert_eq!(parse_yes_no("No", true), Some(false));
        assert_eq!(parse_yes_no("maybe", true), None);
    }
}
