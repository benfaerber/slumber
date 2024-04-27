#![allow(dead_code)]

use std::collections::HashMap;

use nom::{
    branch::alt, bytes::complete::{tag, take_till, take_until}, character::{complete::{alpha1,alphanumeric1, anychar, char, multispace0, multispace1, newline, one_of, space0, space1}, is_space}, combinator::{not, opt, recognize}, error::{Error as NomError, ErrorKind, ParseError, VerboseError}, multi::{many0_count, many1}, sequence::{delimited, pair, tuple}, FindSubstring, Finish, IResult, InputLength, InputTake, Offset, Parser
};
use httparse::{self};

/// Notes:
/// for line in lines:
///     parse_separator: check for ###, check if name exists `### RequestName`
///     parse name annotation: m match `# @name=Name` or `# @name Name`
///     parse_comment: `hello hello // comment`, `hello # comment`, `// comment`, '# comment'
///     parse_variable: @x = y
///     parse_request: build up the request line by line, fill in the variables 


type StrResult<'a> = Result<(&'a str, &'a str), nom::Err<NomError<&'a str>>>;

const REQUEST_DELIMITER: &str = "###";
const NAME_ANNOTATION: &str = "@name";

const VARIABLE_OPEN: &str = "{{";
const VARIABLE_CLOSE: &str = "}}";

#[derive(Debug, Clone, PartialEq)]
enum Line {
    Seperator,
    Name(String),
    Request(String),
}

#[derive(Debug, Clone, PartialEq)]
struct JetbrainsRequest {
    name: Option<String>,
    request: String,
}

impl JetbrainsRequest {
    fn is_empty(&self) -> bool {
        self.request == "" 
    }

    fn push_line(&mut self, value: &str) -> &mut Self {
        self.request.push_str(&format!("{value}\r\n"));
        self
    }

    fn set_name(&mut self, name: &str) -> &mut Self {
        self.name = Some(name.into());
        self
    }

    fn trim(&mut self) -> &mut Self {
        self.request.trim();
        self
    }
}

impl Default for JetbrainsRequest {
    fn default() -> Self {
        Self {
            name: None,
            request: "".into(),
        }
    }
}


#[derive(Debug, Clone, PartialEq)]
struct JetbrainsHttp {
    sections: Vec<JetbrainsRequest>,
    variables: HashMap<String, String>,
}

fn parse_seperator(input: &str) -> IResult<&str, Vec<Line>> {
    let (input, _) = tag(REQUEST_DELIMITER)(input)?;
    let (input, req_name) = opt(pair(
        space1,
        take_till(|c| c == ' ' || c == '\n')
    ))(input)?;

    let got = match req_name {
        Some((_, name)) => vec![Line::Seperator, Line::Name(name.into())],
        None => vec![Line::Seperator],
    };
    Ok((input, got))
}

fn parse_request_name_annotation(input: &str) -> IResult<&str, Line> {
    let (input, _) = pair(char('#'), space0)(input)?;
    let (input, _) = tag(NAME_ANNOTATION)(input)?;
    let (input, _) = pair(alt((char('='), char(' '))), space0)(input)?;
    let (input, req_name) = take_till(|c| c == ' ' || c == '\n')(input)?; 

    Ok((input, Line::Name(req_name.into())))
}

fn parse_variable_identifier(input: &str) -> IResult<&str, &str> {
    recognize(pair(
        alpha1,
        many0_count(alt((alphanumeric1, tag("_"), tag("-"), tag("."))))
    )).parse(input)
}

/// Parses an HTTP File variable (@MY_VAR = 1234)
fn parse_variable_assignment(input: &str) -> IResult<&str, (&str, &str)> {
    let (input, _) = char('@')(input)?;
    let (input, id) = parse_variable_identifier(input)?; 

    let (input, _) = tuple((opt(space1), char('='), opt(space1)))(input)?;
    let (input, value) = take_till(|c| c == '\n')(input)?;
    let (input, _) = newline(input)?;

    Ok((input, (id.into(), value.into())))
}

fn starting_slash_comment(line: &str) -> StrResult {
    tag("//")(line)
}

fn parse_line_without_comment(line: &str) -> StrResult {
    // A comment can start with `//` but it cant be in the middle
    // This would prevent you from writing urls: `https://`
    if let Ok((inp, _)) = starting_slash_comment(line) { 
        return Ok((inp, ""))
    }

    take_until("#")(line)
}

fn parse_variable_substitution(input: &str) -> StrResult {
    let (input, _) = pair(tag(VARIABLE_OPEN), space0)(input)?;
    let (input, id) = parse_variable_identifier(input)?;
    let (input, _) = pair(space0, tag(VARIABLE_CLOSE))(input)?;

    Ok((input, id))
}

fn until_variable_open(input: &str) -> StrResult {
    take_until(VARIABLE_OPEN)(input)
}

fn parse_request_line<'a>(line: &'a str, variables: &'a HashMap<String, String>) -> IResult<&'a str, Line> {
    let mut request_line: String = "".into();
    let mut input = line;
    loop {
        if let Ok((rest, var)) = parse_variable_substitution(input) {
            let value = variables.get(var).unwrap();
            input = rest;
            request_line += value;
            continue
        }

        if let Ok((rest, got)) = until_variable_open(input) { 
            input = rest;
            request_line += got;
            continue;
        }
        
        break;
    }

    Ok((input, Line::Request(request_line.into()))) 
} 


fn parse_lines(input: &str) -> (Vec<Line>, HashMap<String, String>) {
    let mut lines: Vec<Line> = vec![];
    let mut variables: HashMap<String, String> = HashMap::new();
    for line in input.trim().lines() {
        let line = &format!("{line}\n");
        if let Ok((_, sep_lines)) = parse_seperator(line) {
            lines.extend(sep_lines);
            continue;
        }

        if let Ok((_, name)) = parse_request_name_annotation(line) {
            lines.push(name);
            continue;
        }

        let line = parse_line_without_comment(line)
            .map(|(_, without_comment)| without_comment)
            .unwrap_or(line);

        if let Ok((_, (key, val))) = parse_variable_assignment(line) {
            variables.insert(key.into(), val.into());
            continue;
        }

        if line != "\n" {
            lines.push(Line::Request(line.into()));
        }
    }
    (lines, variables)
}

fn build_file_from_lines(lines: Vec<Line>, variables: HashMap<String, String>) -> JetbrainsHttp {
    let mut sections: Vec<JetbrainsRequest> = vec![];
    let mut current_request: JetbrainsRequest = JetbrainsRequest::default();
    for line in lines {
        match line {
            Line::Seperator => {
                if !current_request.is_empty() {
                    current_request.trim();
                    sections.push(current_request);
                }
                current_request = JetbrainsRequest::default();
            },
            Line::Name(name) => {
                current_request.set_name(&name);
            },
            Line::Request(req) => {
                current_request.push_line(&req);
            }
        }
    }

    JetbrainsHttp {
        sections,
        variables,
    }
}


#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parse_http_variable() {
        let example_var = "@MY_VAR    = 1231\n";
        let (_, var) = parse_variable_assignment(example_var).unwrap();

        assert_eq!(
            var,
            ("MY_VAR", "1231"),
        );

        let example_var = "@MY_NAME =hello\n";
        let (rest, var) = parse_variable_assignment(example_var).unwrap();

        assert_eq!(var, ("MY_NAME", "hello"));
        assert_eq!(rest, "");

        let example_var = "@Cool-Word = super_cool\n";
        let (_, var) = parse_variable_assignment(example_var).unwrap();

        assert_eq!(var, ("Cool-Word", "super_cool"));

        println!("{var:?}");
    }

    #[test]
    fn parse_seperator_line() {
        let line = "### RequestName";
        let (_, items) = parse_seperator(line).unwrap();
        assert_eq!(items, vec![Line::Seperator, Line::Name("RequestName".into())]);

        let line = "#######";
        let (_, items) = parse_seperator(line).unwrap();
        assert_eq!(items, vec![Line::Seperator]);

        let line = "###";
        let (_, items) = parse_seperator(line).unwrap();
        assert_eq!(items, vec![Line::Seperator]);

        let line = "#";
        let res = parse_seperator(line);
        assert!(res.is_err());
    }

    #[test]
    fn parse_request_name_test() {
        let line = "# @name=hello";
        let (_, name) = parse_request_name_annotation(line).unwrap();
        assert_eq!(name, Line::Name("hello".into()));  

        let line = "# @name Cool";
        let (_, name) = parse_request_name_annotation(line).unwrap();
        assert_eq!(name, Line::Name("Cool".into()));  
    }


    #[test]
    fn parse_lines_test() {
        let example = r#"
###
@MY_VAR = 123
@hello=blahblah
GET https://httpbin.org HTTP/1.1

// Comment
@var = 12

### Request

GET {{hello}} HTTP/1.1

example example
######
# @name OtherRequest

POST {{HOST}}/post HTTP/1.1
Content-Type: application/json
X-Http-Method-Override: PUT

{
    "data": "my data"
}
        "#;
    
        let (lines, variables) = parse_lines(example);
        println!("{:?}", lines);

        let file = build_file_from_lines(lines, variables);
        println!("{:?}", file);
    }
}