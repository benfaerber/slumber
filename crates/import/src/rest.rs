//! Import request collections from VSCode `.rest` files or Jetbrains `.http` files.

use indexmap::IndexMap;
use slumber_core::{
    collection::{
        self, Authentication, Chain, ChainId, ChainSource, Collection, Folder,
        HasId, Method, Profile, ProfileId, Recipe, RecipeBody, RecipeId,
        RecipeNode, RecipeTree, SelectorMode,
    },
    http::content_type::ContentType,
    template::{Identifier, Template},
    util::NEW_ISSUE_LINK,
};

use rest_parser::{
    headers::Authorization as RestAuthorization,
    template::{Template as RestTemplate, TemplatePart as RestTemplatePart},
    Body as RestBody, RestFlavor, RestFormat, RestRequest, RestVariables,
};
use winnow::Parser;

/// Luckily, rest and slumber use the same template format!
/// Just stringify the rest template and inject it into the slumber template
fn rest_template_to_template(template: RestTemplate) -> Template {
    Template::raw(template.to_string())
}

/// Create a slug from the name
fn slugify(name: &str) -> String {
    name.replace(" ", "_")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

fn build_header_map(
    r_headers: IndexMap<String, RestTemplate>,
) -> IndexMap<String, Template> {
    r_headers
        .into_iter()
        .map(|(k, v)| (k, rest_template_to_template(v)))
        .collect()
}

fn build_authentication(r_auth: RestAuthorization) -> Authentication {
    match r_auth {
        RestAuthorization::Bearer(bearer) => {
            Authentication::Bearer(Template::raw(bearer))
        }
        RestAuthorization::Basic { username, password } => {
            Authentication::Basic {
                username: Template::raw(username),
                password: password.map(|p| Template::raw(p)),
            }
        }
    }
}

fn build_body(
    r_body: RestBody,
    s_headers: IndexMap<String, Template>,
) -> RecipeBody {
    // We only want the text for now
    let body_text = match r_body {
        RestBody::Text(text) => text,
        RestBody::SaveToFile { text, .. } => text,
        RestBody::LoadFromFile { .. } => RestTemplate::new(""),
    };

    let recipe_body = rest_template_to_template(body_text);
    RecipeBody::Raw {
        body: recipe_body,
        content_type: None,
    }
}

fn build_query(
    r_query: IndexMap<String, RestTemplate>,
) -> Vec<(String, Template)> {
    r_query
        .into_iter()
        .map(|(k, v)| (k, rest_template_to_template(v)))
        .collect()
}

fn attempt_receipe(
    request: RestRequest,
    index: usize,
) -> anyhow::Result<Recipe> {
    let RestRequest {
        name: r_name,
        url: r_url,
        query: r_query,
        body: r_body,
        method: r_method,
        headers: r_headers,
        authorization: r_authorization,
        ..
    } = request;
    let s_name = r_name.unwrap_or(format!("Request {index}"));
    let s_method: Method = r_method.to_string().parse().unwrap();
    let s_id = slugify(&s_name);
    let s_url = rest_template_to_template(r_url);
    let s_headers = build_header_map(r_headers);
    let s_auth = r_authorization.map(build_authentication);
    let s_query = build_query(r_query);

    Recipe {
        id: s_id.into(),
        name: s_name.into(),
        method: s_method,
        url: s_url,
        authentication: s_auth,
        body: None,
        headers: s_headers,
        query: s_query, 
    };
    todo!()
}

#[cfg(test)]
mod tests {}
