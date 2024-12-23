//! Import request collections from VSCode `.rest` files or Jetbrains `.http` files.

use anyhow::{anyhow, Context};
use indexmap::IndexMap;
use itertools::Itertools;
use serde::de::IgnoredAny;
use slumber_core::{
    collection::{
        Authentication, Chain, ChainId, ChainOutputTrim, ChainSource,
        Collection, HasId, Method, Profile, ProfileId, Recipe, RecipeBody,
        RecipeId, RecipeNode, RecipeTree, SelectorMode,
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
use std::path::Path;
use tracing::{info, warn};

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

fn rest_template_to_template(template: RestTemplate) -> Template {
    // Rest templates allow spaces in variables
    // For example `{{ HOST}}` or `{{ HOST }}`
    // These must be removed before putting it through the slumber
    // template parser
    let raw_template = template
        .parts
        .iter()
        .map(|part| match part {
            RestTemplatePart::Text(text) => text.to_string(),
            RestTemplatePart::Variable(var) => {
                "{{".to_string() + var.as_str() + "}}"
            }
        })
        .join("");

    raw_template.parse().expect("Invalid template!")
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

    let content_type =
        guess_is_json(headers, variables).then(|| ContentType::Json);
    let recipe_body = RecipeBody::Raw {
        body: template,
        content_type,
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

    let complete_body = request
        .body
        .map(|b| build_body(b, &request.headers, &variables));
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
fn build_profile_map(
    flavor: RestFlavor,
    variables: RestVariables,
) -> IndexMap<ProfileId, Profile> {
    let (flavor_name, flavor_id) = flavor_name_and_id(flavor);
    let profile_id: ProfileId = flavor_id.into();
    let default_profile = Profile {
        id: profile_id.clone(),
        name: Some(flavor_name),
        default: true,
        data: rest_templates_to_slumber_templates(variables),
    };

    IndexMap::from([(profile_id.clone(), default_profile)])
}

fn attempt_collection_from_rest(
    rest_format: RestFormat,
) -> anyhow::Result<Collection> {
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

    Ok(Collection {
        profiles,
        chains,
        recipes,
        _ignore: IgnoredAny,
    })
}

/// Convert a VSCode `.rest` file or a Jetbrains `.http` file into a slumber collection
pub fn from_rest(rest_file: impl AsRef<Path>) -> anyhow::Result<Collection> {
    let rest_file = rest_file.as_ref();

    info!(file = ?rest_file, "Loading Rest collection");
    warn!(
        "The Rest importer is approximate. Some features are missing \
            and it most likely will not give you an equivalent collection. If \
            you would like to request support for a particular Rest \
            feature, please open an issue: {NEW_ISSUE_LINK}"
    );
    // Parse the file and determine the flavor using the extension
    let rest_format = RestFormat::parse_file(rest_file)?;
    let collection = attempt_collection_from_rest(rest_format)?;
    Ok(collection)
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use pretty_assertions::assert_eq;
    use slumber_core::test_util::test_data_dir;

    use super::*;

    fn example_vars() -> RestVariables {
        IndexMap::from([
            ("HOST".into(), RestTemplate::new("https://httpbin.org")),
            ("FIRST_NAME".into(), RestTemplate::new("John")),
            ("LAST_NAME".into(), RestTemplate::new("Smith")),
            (
                "FULL_NAME".into(),
                RestTemplate::new("{{FIRST_NAME}} {{LAST_NAME}}"),
            ),
        ])
    }

    fn test_template(value: &str) -> Template {
        value.parse().unwrap()
    }

    #[test]
    fn can_convert_basic_request() {
        let test_req = RestRequest {
            name: Some("My Request!".into()),
            url: RestTemplate::new("https://httpbin.org"),
            query: IndexMap::from([
                ("name".into(), RestTemplate::new("joe")),
                ("age".into(), RestTemplate::new("46")),
            ]),
            method: RestTemplate::new("GET"),
            ..RestRequest::default()
        };

        let CompleteRecipe { recipe, .. } =
            attempt_receipe(test_req, 0, &IndexMap::new()).unwrap();

        assert_eq!(recipe.url, test_template("https://httpbin.org"));
        assert_eq!(
            recipe.query.get(1).unwrap(),
            &("age".into(), test_template("46"))
        );
        assert_eq!(recipe.method, Method::Get);
        assert_eq!(recipe.id().clone(), RecipeId::from("My_Request__0"));
    }

    #[test]
    fn can_convert_with_vars() {
        let test_req = RestRequest {
            url: RestTemplate::new("{{HOST}}/get"),
            query: IndexMap::from([
                ("first_name".into(), RestTemplate::new("{{FIRST_NAME}}")),
                ("full_name".into(), RestTemplate::new("{{FULL_NAME}}")),
            ]),
            method: RestTemplate::new("POST"),
            ..RestRequest::default()
        };

        let vars = example_vars();
        let CompleteRecipe { recipe, .. } =
            attempt_receipe(test_req, 0, &vars).unwrap();

        assert_eq!(recipe.url, test_template("{{HOST}}/get"));
        assert_eq!(
            recipe.query.get(0).unwrap(),
            &("first_name".into(), test_template("{{FIRST_NAME}}"))
        );
        assert_eq!(
            recipe.query.get(1).unwrap(),
            &("full_name".into(), test_template("{{FULL_NAME}}"))
        );
        assert_eq!(recipe.method, Method::Post);
    }

    #[test]
    fn fails_on_bad_method() {
        let test_req = RestRequest {
            url: RestTemplate::new("{{HOST}}/get"),
            method: RestTemplate::new("INVALID"),
            ..RestRequest::default()
        };

        let got = attempt_receipe(test_req, 0, &IndexMap::new());
        assert!(got.is_err());
    }

    #[test]
    fn can_build_load_chain() {
        let test_req = RestRequest {
            url: RestTemplate::new("{{HOST}}/post"),
            method: RestTemplate::new("POST"),
            headers: IndexMap::from([(
                "Content-Type".into(),
                RestTemplate::new("application/json"),
            )]),
            body: Some(RestBody::LoadFromFile {
                process_variables: false,
                encoding: None,
                filepath: RestTemplate::new("./test_data/rest_pets.json"),
            }),
            ..RestRequest::default()
        };

        let CompleteRecipe { chain, .. } =
            attempt_receipe(test_req, 0, &IndexMap::new()).unwrap();

        let chain = chain.unwrap();
        assert_eq!(
            chain.id().clone(),
            ChainId::from("__test_data_rest_pets_json")
        );
        let expected_source = ChainSource::File {
            path: Template::raw("./test_data/rest_pets.json".into()),
        };
        assert_eq!(chain.source, expected_source);
        assert_eq!(chain.content_type, Some(ContentType::Json));
    }

    #[test]
    fn can_build_raw_body() {
        let test_req = RestRequest {
            url: RestTemplate::new("{{HOST}}/post"),
            method: RestTemplate::new("POST"),
            body: Some(RestBody::Text(RestTemplate::new("test data"))),
            ..RestRequest::default()
        };

        let CompleteRecipe { recipe, .. } =
            attempt_receipe(test_req, 0, &example_vars()).unwrap();

        let body = recipe.body.unwrap();
        assert_eq!(
            body,
            RecipeBody::Raw {
                body: test_template("test data"),
                content_type: None,
            }
        );
    }

    #[test]
    fn can_build_json_body() {
        let test_req = RestRequest {
            url: RestTemplate::new("{{HOST}}/post"),
            method: RestTemplate::new("POST"),
            headers: IndexMap::from([(
                "Content-Type".into(),
                RestTemplate::new("application/json"),
            )]),
            body: Some(RestBody::Text(RestTemplate::new(
                "{\"animal\": \"penguin\"}",
            ))),
            ..RestRequest::default()
        };

        let CompleteRecipe { recipe, .. } =
            attempt_receipe(test_req, 0, &example_vars()).unwrap();

        let body = recipe.body.unwrap();
        assert_eq!(
            body,
            RecipeBody::Raw {
                body: test_template("{\"animal\": \"penguin\"}"),
                content_type: Some(ContentType::Json),
            }
        );
    }

    #[test]
    fn can_build_collection_from_rest_format() {
        let test_req_1 = RestRequest {
            name: Some("Query Request".into()),
            url: RestTemplate::new("https://httpbin.org"),
            query: IndexMap::from([
                ("name".into(), RestTemplate::new("joe")),
                ("age".into(), RestTemplate::new("46")),
            ]),
            method: RestTemplate::new("GET"),
            ..RestRequest::default()
        };

        let test_req_2 = RestRequest {
            url: RestTemplate::new("{{HOST}}/post"),
            method: RestTemplate::new("POST"),
            headers: IndexMap::from([(
                "Content-Type".into(),
                RestTemplate::new("application/json"),
            )]),
            body: Some(RestBody::Text(RestTemplate::new(
                "{\"animal\": \"penguin\"}",
            ))),
            ..RestRequest::default()
        };

        let format = RestFormat {
            requests: vec![test_req_1, test_req_2],
            flavor: RestFlavor::Jetbrains,
            variables: example_vars(),
        };

        let Collection { recipes, .. } =
            attempt_collection_from_rest(format).unwrap();

        let recipe_1 = recipes.get(&RecipeId::from("Query_Request_0")).unwrap();
        let recipe_2 = recipes.get(&RecipeId::from("Request_1")).unwrap();

        println!("{recipe_1:?}");
        match (recipe_1, recipe_2) {
            (
                RecipeNode::Recipe(Recipe { body: body1, .. }),
                RecipeNode::Recipe(Recipe { body: body2, .. }),
            ) => {
                assert_eq!(body1, &None,);
                assert_eq!(
                    body2,
                    &Some(RecipeBody::Raw {
                        body: test_template("{\"animal\": \"penguin\"}"),
                        content_type: Some(ContentType::Json),
                    })
                );
            }
            _ => panic!("Invalid! {recipe_1:?} {recipe_2:?}"),
        }
    }

    #[test]
    fn can_load_collection_from_file() {
        let test_path = test_data_dir().join("rest_http_bin.http");
        let Collection {
            recipes,
            chains,
            profiles,
            ..
        } = from_rest(test_path).unwrap();

        // First Recipe
        let first_recipe = recipes.get(&RecipeId::from("SimpleGet_0")).unwrap();

        if let RecipeNode::Recipe(recipe) = first_recipe {
            assert_eq!(recipe.method, Method::Get);
            assert_eq!(recipe.url, test_template("{{HOST}}/get"),);
        } else {
            panic!("Should be recipe node");
        }

        // Second recipe
        let second_recipe = recipes.get(&RecipeId::from("JsonPost_1")).unwrap();

        if let RecipeNode::Recipe(recipe) = second_recipe {
            assert_eq!(recipe.method, Method::Post,);
            assert_eq!(recipe.url, test_template("{{HOST}}/post"),);

            let json_body = r#"{
"data": "my data",
"name": "{{FULL}}"
}"#
            .replace("\n", "\r\n");
            assert_eq!(
                recipe.body,
                Some(RecipeBody::Raw {
                    body: test_template(&json_body),
                    content_type: Some(ContentType::Json),
                })
            );
        } else {
            panic!("Should be recipe node");
        }

        // Profile variables
        let Profile { data, .. } =
            profiles.get(&ProfileId::from("http_file")).unwrap();
        println!("{data:?}");
        
        let host = data.get("HOST").unwrap();
        let first = data.get("FIRST").unwrap();
        let last = data.get("LAST").unwrap();
        let full = data.get("FULL").unwrap();

        assert_eq!(host, &test_template("http://httpbin.org"));
        assert_eq!(first, &test_template("Joe"));
        assert_eq!(last, &test_template("Smith"));
        assert_eq!(full, &test_template("{{FIRST}} {{LAST}}"));

        // Load Chain
        let chain = chains.get(&ChainId::from("__test_data_rest_pets_json")).unwrap();
        let expected_source = ChainSource::File {
            path: Template::raw("./test_data/rest_pets.json".into()),
        };
        assert_eq!(chain.source, expected_source);
        assert_eq!(chain.content_type, Some(ContentType::Json));
    }
}
