//! Import request collections from VSCode `.rest` files or Jetbrains `.http` files.

use anyhow::{anyhow, Context};
use indexmap::IndexMap;
use serde::de::IgnoredAny;
use slumber_core::{
    collection::{
        Authentication, Chain, ChainId, ChainOutputTrim, ChainSource,
        Collection, HasId, Method, Profile,
        ProfileId, Recipe, RecipeBody, RecipeId, RecipeNode, RecipeTree,
        SelectorMode,
    },
    http::content_type::ContentType,
    template::{Identifier, Template}, util::NEW_ISSUE_LINK,
};

use rest_parser::{
    headers::Authorization as RestAuthorization,
    template::{Template as RestTemplate, TemplatePart as RestTemplatePart},
    Body as RestBody, RestFlavor, RestFormat, RestRequest, RestVariables,
};
use tracing::{info, warn};
use std::{fs::File, io::Read, str::FromStr};
use std::path::Path;

/// Case insensitive header to use to guess the content type
const HEADER_CONTENT_TYPE: &'static str = "content-type";

/// Case insensitive content type to assume a request is JSON
const CONTENT_TYPE_JSON: &'static str = "application/json";

/// In Rest "Chains" and "Requests" are connected
/// The only chain is loading from a file
struct CompleteRecipe {
    recipe: Recipe,
    chain: Option<Chain>,
}

struct CompleteBody {
    recipe_body: RecipeBody,
    chain: Option<Chain>,
}

/// Luckily, rest and slumber use the same template format!
/// Just stringify the rest template and inject it into the slumber template
fn rest_template_to_template(template: RestTemplate) -> Template {
    Template::raw(template.to_string())
}

fn rest_templates_to_slumber_templates(
    template_map: IndexMap<String, RestTemplate>,
) -> IndexMap<String, Template> {
    template_map
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

fn chain_from_load_body(
    filepath: RestTemplate,
    headers: &IndexMap<String, RestTemplate>,
    variables: &RestVariables,
) -> Chain {
    let rendered_filepath = filepath.to_string();

    let id: Identifier = Identifier::escape(&rendered_filepath);

    let path = rest_template_to_template(filepath);

    let content_type =
        guess_is_json(headers, variables).then(|| ContentType::Json);

    Chain {
        id: id.into(),
        content_type,
        trim: ChainOutputTrim::None,
        source: ChainSource::File { path },
        sensitive: false,
        selector: None,
        selector_mode: SelectorMode::Single,
    }
}

/// Attempt to use headers to determine if the request is JSON
fn guess_is_json(
    r_headers: &IndexMap<String, RestTemplate>,
    variables: &RestVariables,
) -> bool {
    for (name, value) in r_headers {
        if name.to_lowercase() == HEADER_CONTENT_TYPE {
            return value.render(variables) == CONTENT_TYPE_JSON;
        }
    }
    false
}

fn build_body(
    body: RestBody,
    headers: &IndexMap<String, RestTemplate>,
    variables: &RestVariables,
) -> CompleteBody {
    // We only want the text for now
    let (template, chain) = match body {
        RestBody::Text(text) => (rest_template_to_template(text), None),
        RestBody::SaveToFile { text, .. } => {
            (rest_template_to_template(text), None)
        }
        RestBody::LoadFromFile { filepath, .. } => {
            let chain = chain_from_load_body(filepath, headers, variables);
            let template = Template::from_chain(chain.id().clone());
            (template, Some(chain))
        }
    };

    let recipe_body = RecipeBody::Raw {
        body: template,
        content_type: None,
    };

    CompleteBody { recipe_body, chain }
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
    variables: &RestVariables,
) -> anyhow::Result<CompleteRecipe> {
    let name = request.name.unwrap_or(format!("Request"));

    let slug = Identifier::escape(&name);
    // Add the index to prevent duplicate ID error
    let id = format!("{slug}_{index}");

    // Slumber doesn't support template methods, so we fill in now
    let rendered_method = request.method.render(variables);

    // The rest parser does not enforce method names
    // It must be checked here
    let method: Method = rendered_method
        .parse()
        .map_err(|_| anyhow!("Unsupported method: {:?}!", request.method))?;
    let url = rest_template_to_template(request.url);
    let authentication = request.authorization.map(build_authentication);
    let query = build_query(request.query);

    let complete_body = request.body.map(|b| build_body(b, &request.headers, &variables));
    let (body, chain) = if let Some(complete) = complete_body {
        (Some(complete.recipe_body), complete.chain)
    } else {
        (None, None)
    };

    let headers = rest_templates_to_slumber_templates(request.headers);

    let recipe = Recipe {
        id: id.into(),
        name: name.into(),
        method,
        url,
        authentication,
        body,
        headers,
        query,
    };

    Ok(CompleteRecipe { recipe, chain })
}

/// Rest has no request nesting feature so a tree will always be flat
fn build_recipe_tree_with_chains(
    completed: Vec<CompleteRecipe>,
) -> (RecipeTree, IndexMap<ChainId, Chain>) {
    let mut chains: IndexMap<ChainId, Chain> = IndexMap::new();
    let recipe_node_map = completed
        .into_iter()
        .map(|CompleteRecipe { recipe, chain }| {
            if let Some(load_chain) = chain {
                chains.insert(load_chain.id().clone(), load_chain);
            }

            (recipe.id().clone(), RecipeNode::Recipe(recipe))
        })
        .collect::<IndexMap<RecipeId, RecipeNode>>();

    let recipe_tree = RecipeTree::new(recipe_node_map)
        .expect("IDs are injected by the recipe converter!");

    (recipe_tree, chains)
}


fn flavor_name_and_id(flavor: RestFlavor) -> (String, String) {
    let (name, id) = match flavor {
        RestFlavor::Jetbrains => ("Jetbrains HTTP File", "http_file"),
        RestFlavor::Vscode => ("VSCode Rest File", "rest_file"),
        RestFlavor::Generic => ("Rest File", "rest_file"),
    };
    (name.into(), id.into())
} 


/// There is no profile system in Rest,
/// here is a default to use
fn build_profile_map(flavor: RestFlavor, variables: RestVariables) -> IndexMap<ProfileId, Profile> {
    let (flavor_name, flavor_id) = flavor_name_and_id(flavor);
    let profile_id: ProfileId = flavor_id.into(); 
    let default_profile = Profile {
        id: profile_id.clone(),
        name: Some(flavor_name),
        default: true,
        data: rest_templates_to_slumber_templates(variables) 
    };

    IndexMap::from([
        (profile_id.clone(), default_profile)
    ])
} 


fn attempt_collection_from_rest(rest_format: RestFormat) -> anyhow::Result<Collection> {
    let RestFormat {
        requests,
        variables,
        flavor,
    } = rest_format;

    let completed_recipes = requests
        .into_iter()
        .enumerate()
        .map(|(index, req)| attempt_receipe(req, index, &variables).unwrap())
        .collect::<Vec<_>>();

    let (recipes, chains) = build_recipe_tree_with_chains(completed_recipes);

    let profiles = build_profile_map(flavor, variables);

    Collection {
        profiles,
        chains,
        recipes,
        _ignore: IgnoredAny,
    };

    todo!()
}


/// Convert a VSCode `.rest` file or a Jetbrains `.http` file into a slumber collection
pub fn from_rest(
    rest_file: impl AsRef<Path>,
) -> anyhow::Result<Collection> {
    let rest_file = rest_file.as_ref();
    // Parse the file and determine the flavor using the extension 
    let rest_format = RestFormat::parse_file(rest_file)?;
    let collection = attempt_collection_from_rest(rest_format)?;
    Ok(collection)
}

#[cfg(test)]
mod tests {

    fn parse_http_bin_file_test() {

    }


}
