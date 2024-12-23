use crate::{GlobalArgs, Subcommand};
use anyhow::Context;
use clap::{Parser, ValueEnum};
use std::{
    fs::File,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

/// Generate a Slumber request collection from an external format
#[derive(Clone, Debug, Parser)]
pub struct ImportCommand {
    /// Input format
    format: Format,
    /// Collection to import
    input_file: PathBuf,
    /// Destination for the new slumber collection file [default: stdout]
    output_file: Option<PathBuf>,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
#[allow(rustdoc::bare_urls)]
enum Format {
    /// Insomnia export format (JSON or YAML)
    Insomnia,
    /// OpenAPI v3.0 (JSON or YAML) v3.1 not supported but may work
    /// https://spec.openapis.org/oas/v3.0.3
    Openapi,
    /// A VSCode `.rest` file or a Jetbrains `.http` file
    /// https://github.com/Huachao/vscode-restclient
    /// https://www.jetbrains.com/help/idea/http-client-in-product-code-editor.html
    Rest,
    /// Makes the importer more user friendly
    /// The end user doesn't need to know VSCode and Jetbrains are treated the same
    /// under the hood
    Vscode,
    Jetbrains,
}

impl Subcommand for ImportCommand {
    async fn execute(self, _global: GlobalArgs) -> anyhow::Result<ExitCode> {
        // Load the input
        let collection = match self.format {
            Format::Insomnia => {
                slumber_import::from_insomnia(&self.input_file)?
            }
            Format::Openapi => slumber_import::from_openapi(&self.input_file)?,
            Format::Rest | Format::Vscode | Format::Jetbrains => slumber_import::from_rest(&self.input_file)?,
        };

        // Write the output
        let mut writer: Box<dyn Write> = match self.output_file {
            Some(output_file) => Box::new(
                File::options()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&output_file)
                    .context(format!(
                        "Error opening collection output file \
                        {output_file:?}"
                    ))?,
            ),
            None => Box::new(io::stdout()),
        };
        serde_yaml::to_writer(&mut writer, &collection)?;

        Ok(ExitCode::SUCCESS)
    }
}
