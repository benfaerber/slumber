use crate::{
    context::TuiContext,
    view::{
        common::{tabs::Tabs, template_preview::TemplatePreview},
        component::recipe_pane::{
            authentication::AuthenticationDisplay,
            body::RecipeBodyDisplay,
            persistence::RecipeOverrideKey,
            table::{RecipeFieldTable, RecipeFieldTableProps},
        },
        draw::{Draw, DrawMetadata},
        event::{Child, EventHandler},
        state::Identified,
        util::persistence::PersistedLazy,
        Component,
    },
};
use derive_more::Display;
use persisted::SingletonKey;
use ratatui::{
    layout::{Alignment, Layout},
    prelude::Constraint,
    text::{Span, Text},
    widgets::Paragraph,
    Frame,
};
use serde::{Deserialize, Serialize};
use slumber_config::Action;
use slumber_core::{
    collection::{Method, Recipe, RecipeId},
    http::BuildOptions,
};
use std::ops::Deref;
use strum::{EnumCount, EnumIter};

/// Display a recipe. Note a recipe *node*, this is for genuine bonafide recipe.
/// This maintains internal state specific to a recipe, so it should be
/// recreated every time the recipe/profile changes.
#[derive(Debug)]
pub struct RecipeDisplay {
    tabs: Component<PersistedLazy<SingletonKey<Tab>, Tabs<Tab>>>,
    url: TemplatePreview,
    method: Method,
    query: Component<RecipeFieldTable<QueryRowKey, QueryRowToggleKey>>,
    headers: Component<RecipeFieldTable<HeaderRowKey, HeaderRowToggleKey>>,
    body: Component<Option<RecipeBodyDisplay>>,
    authentication: Component<Option<AuthenticationDisplay>>,
}

impl RecipeDisplay {
    /// Initialize new recipe state. Should be called whenever the recipe or
    /// profile changes
    pub fn new(recipe: &Recipe) -> Self {
        Self {
            tabs: Default::default(),
            method: recipe.method,
            url: TemplatePreview::new(recipe.url.clone(), None),
            query: RecipeFieldTable::new(
                QueryRowKey(recipe.id.clone()),
                recipe.query.iter().enumerate().map(|(i, (param, value))| {
                    (
                        param.clone(),
                        value.clone(),
                        RecipeOverrideKey::query_param(recipe.id.clone(), i),
                        QueryRowToggleKey {
                            recipe_id: recipe.id.clone(),
                            param: param.clone(),
                        },
                    )
                }),
            )
            .into(),
            headers: RecipeFieldTable::new(
                HeaderRowKey(recipe.id.clone()),
                recipe.headers.iter().enumerate().map(
                    |(i, (header, value))| {
                        (
                            header.clone(),
                            value.clone(),
                            RecipeOverrideKey::header(recipe.id.clone(), i),
                            HeaderRowToggleKey {
                                recipe_id: recipe.id.clone(),
                                header: header.clone(),
                            },
                        )
                    },
                ),
            )
            .into(),
            body: recipe
                .body
                .as_ref()
                .map(|body| RecipeBodyDisplay::new(body, recipe.id.clone()))
                .into(),
            // Map authentication type
            authentication: recipe
                .authentication
                .as_ref()
                .map(|authentication| {
                    AuthenticationDisplay::new(
                        recipe.id.clone(),
                        authentication.clone(),
                    )
                })
                .into(),
        }
    }

    /// Generate a [BuildOptions] instance based on current UI state
    pub fn build_options(&self) -> BuildOptions {
        let authentication = self
            .authentication
            .data()
            .as_ref()
            .and_then(|authentication| authentication.override_value());
        let form_fields = self
            .body
            .data()
            .as_ref()
            .and_then(|body| match body {
                RecipeBodyDisplay::Raw(_) => None,
                RecipeBodyDisplay::Form(form) => {
                    Some(form.data().to_build_overrides())
                }
            })
            .unwrap_or_default();
        let body = self
            .body
            .data()
            .as_ref()
            .and_then(|body| body.override_value());

        BuildOptions {
            authentication,
            headers: self.headers.data().to_build_overrides(),
            query_parameters: self.query.data().to_build_overrides(),
            form_fields,
            body,
        }
    }

    /// Does the recipe have a body defined?
    pub fn has_body(&self) -> bool {
        self.body.data().is_some()
    }

    /// Get visible body text
    pub fn body_text(
        &self,
    ) -> Option<impl '_ + Deref<Target = Identified<Text<'static>>>> {
        let body = self.body.data().as_ref()?;
        body.text()
    }
}

impl EventHandler for RecipeDisplay {
    fn children(&mut self) -> Vec<Component<Child<'_>>> {
        vec![
            self.tabs.to_child_mut(),
            self.body.to_child_mut(),
            self.query.to_child_mut(),
            self.headers.to_child_mut(),
            self.authentication.to_child_mut(),
        ]
    }
}

impl Draw for RecipeDisplay {
    fn draw(&self, frame: &mut Frame, _: (), metadata: DrawMetadata) {
        let tui_context = TuiContext::get();

        // Render request contents
        let method = self.method.to_string();

        let [metadata_area, tabs_area, content_area, footer_area] =
            Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(metadata.area());

        let [method_area, url_area] = Layout::horizontal(
            // Method gets just as much as it needs, URL gets the rest
            [Constraint::Max(method.len() as u16 + 1), Constraint::Min(0)],
        )
        .areas(metadata_area);

        // First line: Method + URL
        frame.render_widget(Paragraph::new(method), method_area);
        frame.render_widget(&self.url, url_area);

        // Navigation tabs
        self.tabs.draw(frame, (), tabs_area, true);

        // Helper footer
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    "Press {} to edit value, {} to reset",
                    tui_context.input_engine.binding_display(Action::Edit),
                    tui_context.input_engine.binding_display(Action::Reset),
                ),
                tui_context.styles.text.hint,
            ))
            .alignment(Alignment::Right),
            footer_area,
        );

        // Recipe content
        match self.tabs.data().selected() {
            Tab::Body => self.body.draw_opt(frame, (), content_area, true),
            Tab::Query => self.query.draw(
                frame,
                RecipeFieldTableProps {
                    key_header: "Parameter",
                    value_header: "Value",
                },
                content_area,
                true,
            ),
            Tab::Headers => self.headers.draw(
                frame,
                RecipeFieldTableProps {
                    key_header: "Header",
                    value_header: "Value",
                },
                content_area,
                true,
            ),
            Tab::Authentication => {
                self.authentication.draw_opt(frame, (), content_area, true)
            }
        }
    }
}

#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Display,
    EnumCount,
    EnumIter,
    PartialEq,
    Serialize,
    Deserialize,
)]
enum Tab {
    #[default]
    Body,
    Query,
    Headers,
    Authentication,
}

/// Persistence key for selected query param, per recipe. Value is the query
/// param name
#[derive(Debug, Serialize, persisted::PersistedKey)]
#[persisted(Option<String>)]
struct QueryRowKey(RecipeId);

/// Persistence key for toggle state for a single query param in the table
#[derive(Debug, Serialize, persisted::PersistedKey)]
#[persisted(bool)]
struct QueryRowToggleKey {
    recipe_id: RecipeId,
    param: String,
}

/// Persistence key for selected header, per recipe. Value is the header name
#[derive(Debug, Serialize, persisted::PersistedKey)]
#[persisted(Option<String>)]
struct HeaderRowKey(RecipeId);

/// Persistence key for toggle state for a single header in the table
#[derive(Debug, Serialize, persisted::PersistedKey)]
#[persisted(bool)]
struct HeaderRowToggleKey {
    recipe_id: RecipeId,
    header: String,
}
