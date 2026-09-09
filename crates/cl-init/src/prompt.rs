//! Question asking for `clouduler init`.
//!
//! Every question takes a default, and a non-interactive prompter answers with
//! it without reading anything. That is what makes one code path serve both the
//! guided setup and the scripted one: a question nobody can answer is an error
//! rather than a hang.

use std::io::{BufRead, Write};

use anyhow::{bail, Result};

pub struct Prompt<R, W> {
    input: R,
    output: W,
    interactive: bool,
}

impl<R: BufRead, W: Write> Prompt<R, W> {
    pub fn new(input: R, output: W, interactive: bool) -> Self {
        Self {
            input,
            output,
            interactive,
        }
    }

    /// Print a line of context, not a question.
    pub fn say(&mut self, line: &str) -> Result<()> {
        writeln!(self.output, "{line}")?;
        Ok(())
    }

    pub fn heading(&mut self, title: &str) -> Result<()> {
        writeln!(self.output, "\n== {title}")?;
        Ok(())
    }

    /// Ask for a value. An empty answer takes the default; without a default an
    /// empty answer is re-asked, because the caller needs something.
    pub fn ask(&mut self, question: &str, default: Option<&str>) -> Result<String> {
        loop {
            match self.ask_optional(question, default)? {
                Some(answer) => return Ok(answer),
                None => {
                    if !self.interactive {
                        bail!("{question}: no answer and no default, pass it as an argument");
                    }
                    self.say("  that one is required")?;
                }
            }
        }
    }

    /// Ask for a value that may legitimately be left unset.
    pub fn ask_optional(&mut self, question: &str, default: Option<&str>) -> Result<Option<String>> {
        if !self.interactive {
            return Ok(default.map(str::to_string));
        }

        match default {
            Some(default) => write!(self.output, "{question} [{default}]: ")?,
            None => write!(self.output, "{question} []: ")?,
        }
        self.output.flush()?;

        let line = self.read_line()?;
        let answer = line.trim();
        if answer.is_empty() {
            return Ok(default.map(str::to_string));
        }
        Ok(Some(answer.to_string()))
    }

    pub fn confirm(&mut self, question: &str, default: bool) -> Result<bool> {
        if !self.interactive {
            return Ok(default);
        }

        loop {
            let hint = if default { "Y/n" } else { "y/N" };
            write!(self.output, "{question} [{hint}]: ")?;
            self.output.flush()?;

            match self.read_line()?.trim().to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.say("  answer y or n")?,
            }
        }
    }

    /// Pick one of a fixed set, by number or by name. Returns the index.
    pub fn choose(&mut self, question: &str, options: &[&str], default: usize) -> Result<usize> {
        if !self.interactive {
            return Ok(default);
        }

        loop {
            writeln!(self.output, "{question}")?;
            for (i, option) in options.iter().enumerate() {
                let marker = if i == default { "*" } else { " " };
                writeln!(self.output, " {marker} {}) {option}", i + 1)?;
            }
            write!(self.output, "choice [{}]: ", options[default])?;
            self.output.flush()?;

            let line = self.read_line()?;
            let answer = line.trim();
            if answer.is_empty() {
                return Ok(default);
            }
            if let Ok(number) = answer.parse::<usize>() {
                if (1..=options.len()).contains(&number) {
                    return Ok(number - 1);
                }
            }
            if let Some(i) = options.iter().position(|o| o.eq_ignore_ascii_case(answer)) {
                return Ok(i);
            }
            self.say("  not one of the options")?;
        }
    }

    /// End of input is treated as "take the default" rather than an error, so a
    /// piped script that stops early still finishes with sane answers.
    fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            self.interactive = false;
        }
        Ok(line)
    }
}

/// The prompter the command actually runs with.
pub fn stdio(interactive: bool) -> Prompt<std::io::BufReader<std::io::Stdin>, std::io::Stdout> {
    Prompt::new(
        std::io::BufReader::new(std::io::stdin()),
        std::io::stdout(),
        interactive,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scripted(script: &str) -> Prompt<std::io::Cursor<Vec<u8>>, Vec<u8>> {
        Prompt::new(
            std::io::Cursor::new(script.as_bytes().to_vec()),
            Vec::new(),
            true,
        )
    }

    #[test]
    fn an_empty_answer_takes_the_default() {
        let mut prompt = scripted("\n");
        assert_eq!(prompt.ask("name", Some("n1")).unwrap(), "n1");
    }

    #[test]
    fn a_typed_answer_wins_over_the_default() {
        let mut prompt = scripted("n7\n");
        assert_eq!(prompt.ask("name", Some("n1")).unwrap(), "n7");
    }

    #[test]
    fn a_required_question_is_asked_again_until_answered() {
        let mut prompt = scripted("\n\nn1\n");
        assert_eq!(prompt.ask("name", None).unwrap(), "n1");
    }

    #[test]
    fn an_optional_question_may_be_left_blank() {
        let mut prompt = scripted("\n");
        assert_eq!(prompt.ask_optional("domain", None).unwrap(), None);
    }

    #[test]
    fn confirm_reads_yes_and_no_in_either_spelling() {
        let mut prompt = scripted("yes\nn\n\n");
        assert!(prompt.confirm("go", false).unwrap());
        assert!(!prompt.confirm("go", true).unwrap());
        assert!(prompt.confirm("go", true).unwrap());
    }

    #[test]
    fn choose_accepts_a_number_or_the_option_name() {
        let mut prompt = scripted("2\nmanager\n\n");
        let options = ["manager", "worker"];
        assert_eq!(prompt.choose("role", &options, 0).unwrap(), 1);
        assert_eq!(prompt.choose("role", &options, 1).unwrap(), 0);
        assert_eq!(prompt.choose("role", &options, 1).unwrap(), 1);
    }

    #[test]
    fn a_non_interactive_prompt_answers_with_the_default_and_reads_nothing() {
        let mut prompt = Prompt::new(std::io::Cursor::new(b"ignored\n".to_vec()), Vec::new(), false);
        assert_eq!(prompt.ask("name", Some("n1")).unwrap(), "n1");
        assert!(prompt.confirm("go", true).unwrap());
    }

    #[test]
    fn a_non_interactive_prompt_fails_on_a_question_it_cannot_answer() {
        let mut prompt = Prompt::new(std::io::Cursor::new(Vec::new()), Vec::new(), false);
        assert!(prompt.ask("subnet", None).is_err());
    }

    #[test]
    fn input_ending_early_falls_back_to_the_defaults() {
        let mut prompt = scripted("");
        assert_eq!(prompt.ask("name", Some("n1")).unwrap(), "n1");
        assert!(prompt.confirm("go", true).unwrap());
    }
}
