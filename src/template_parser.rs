// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::num::ParseIntError;
use std::ops::{RangeFrom, RangeInclusive};
use std::{error, fmt};

use itertools::Itertools as _;
use jujutsu_lib::backend::{Signature, Timestamp};
use jujutsu_lib::commit::Commit;
use jujutsu_lib::op_store::WorkspaceId;
use jujutsu_lib::repo::RepoRef;
use jujutsu_lib::rewrite;
use pest::iterators::{Pair, Pairs};
use pest::Parser;
use pest_derive::Parser;
use thiserror::Error;

use crate::templater::{
    BranchProperty, CommitOrChangeId, ConditionalTemplate, FormattablePropertyTemplate,
    GitHeadProperty, GitRefsProperty, LabelTemplate, ListTemplate, Literal,
    PlainTextFormattedProperty, SeparateTemplate, ShortestIdPrefix, TagProperty, Template,
    TemplateFunction, TemplateProperty, TemplatePropertyFn, WorkingCopiesProperty,
};
use crate::{cli_util, time_util};

#[derive(Parser)]
#[grammar = "template.pest"]
struct TemplateParser;

type TemplateParseResult<T> = Result<T, TemplateParseError>;

#[derive(Clone, Debug)]
pub struct TemplateParseError {
    kind: TemplateParseErrorKind,
    pest_error: Box<pest::error::Error<Rule>>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TemplateParseErrorKind {
    #[error("Syntax error")]
    SyntaxError,
    #[error("Invalid integer literal: {0}")]
    ParseIntError(#[source] ParseIntError),
    #[error(r#"Keyword "{0}" doesn't exist"#)]
    NoSuchKeyword(String),
    #[error(r#"Function "{0}" doesn't exist"#)]
    NoSuchFunction(String),
    #[error(r#"Method "{name}" doesn't exist for type "{type_name}""#)]
    NoSuchMethod { type_name: String, name: String },
    // TODO: clean up argument error variants
    #[error("Expected {0} arguments")]
    InvalidArgumentCountExact(usize),
    #[error("Expected {} to {} arguments", .0.start(), .0.end())]
    InvalidArgumentCountRange(RangeInclusive<usize>),
    #[error("Expected at least {} arguments", .0.start)]
    InvalidArgumentCountRangeFrom(RangeFrom<usize>),
    #[error(r#"Expected argument of type "{0}""#)]
    InvalidArgumentType(String),
}

impl TemplateParseError {
    fn with_span(kind: TemplateParseErrorKind, span: pest::Span<'_>) -> Self {
        let pest_error = Box::new(pest::error::Error::new_from_span(
            pest::error::ErrorVariant::CustomError {
                message: kind.to_string(),
            },
            span,
        ));
        TemplateParseError { kind, pest_error }
    }

    fn no_such_keyword(name: impl Into<String>, span: pest::Span<'_>) -> Self {
        TemplateParseError::with_span(TemplateParseErrorKind::NoSuchKeyword(name.into()), span)
    }

    fn no_such_function(function: &FunctionCallNode) -> Self {
        TemplateParseError::with_span(
            TemplateParseErrorKind::NoSuchFunction(function.name.to_owned()),
            function.name_span,
        )
    }

    fn no_such_method(type_name: impl Into<String>, function: &FunctionCallNode) -> Self {
        TemplateParseError::with_span(
            TemplateParseErrorKind::NoSuchMethod {
                type_name: type_name.into(),
                name: function.name.to_owned(),
            },
            function.name_span,
        )
    }

    fn invalid_argument_count_exact(count: usize, span: pest::Span<'_>) -> Self {
        TemplateParseError::with_span(
            TemplateParseErrorKind::InvalidArgumentCountExact(count),
            span,
        )
    }

    fn invalid_argument_count_range(count: RangeInclusive<usize>, span: pest::Span<'_>) -> Self {
        TemplateParseError::with_span(
            TemplateParseErrorKind::InvalidArgumentCountRange(count),
            span,
        )
    }

    fn invalid_argument_count_range_from(count: RangeFrom<usize>, span: pest::Span<'_>) -> Self {
        TemplateParseError::with_span(
            TemplateParseErrorKind::InvalidArgumentCountRangeFrom(count),
            span,
        )
    }

    fn invalid_argument_type(expected_type_name: impl Into<String>, span: pest::Span<'_>) -> Self {
        TemplateParseError::with_span(
            TemplateParseErrorKind::InvalidArgumentType(expected_type_name.into()),
            span,
        )
    }
}

impl From<pest::error::Error<Rule>> for TemplateParseError {
    fn from(err: pest::error::Error<Rule>) -> Self {
        TemplateParseError {
            kind: TemplateParseErrorKind::SyntaxError,
            pest_error: Box::new(err),
        }
    }
}

impl fmt::Display for TemplateParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.pest_error.fmt(f)
    }
}

impl error::Error for TemplateParseError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match &self.kind {
            // SyntaxError is a wrapper for pest::error::Error.
            TemplateParseErrorKind::SyntaxError => Some(&self.pest_error as &dyn error::Error),
            // Otherwise the kind represents this error.
            e => e.source(),
        }
    }
}

/// AST node without type or name checking.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpressionNode<'i> {
    kind: ExpressionKind<'i>,
    span: pest::Span<'i>,
}

impl<'i> ExpressionNode<'i> {
    fn new(kind: ExpressionKind<'i>, span: pest::Span<'i>) -> Self {
        ExpressionNode { kind, span }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum ExpressionKind<'i> {
    Identifier(&'i str),
    Integer(i64),
    String(String),
    List(Vec<ExpressionNode<'i>>),
    FunctionCall(FunctionCallNode<'i>),
    MethodCall(MethodCallNode<'i>),
}

#[derive(Clone, Debug, PartialEq)]
struct FunctionCallNode<'i> {
    name: &'i str,
    name_span: pest::Span<'i>,
    args: Vec<ExpressionNode<'i>>,
    args_span: pest::Span<'i>,
}

#[derive(Clone, Debug, PartialEq)]
struct MethodCallNode<'i> {
    object: Box<ExpressionNode<'i>>,
    function: FunctionCallNode<'i>,
}

fn parse_string_literal(pair: Pair<Rule>) -> String {
    assert_eq!(pair.as_rule(), Rule::literal);
    let mut result = String::new();
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::raw_literal => {
                result.push_str(part.as_str());
            }
            Rule::escape => match part.as_str().as_bytes()[1] as char {
                '"' => result.push('"'),
                '\\' => result.push('\\'),
                'n' => result.push('\n'),
                char => panic!("invalid escape: \\{char:?}"),
            },
            _ => panic!("unexpected part of string: {part:?}"),
        }
    }
    result
}

fn parse_function_call_node(pair: Pair<Rule>) -> TemplateParseResult<FunctionCallNode> {
    assert_eq!(pair.as_rule(), Rule::function);
    let mut inner = pair.into_inner();
    let name = inner.next().unwrap();
    let args_pair = inner.next().unwrap();
    let args_span = args_pair.as_span();
    assert_eq!(name.as_rule(), Rule::identifier);
    assert_eq!(args_pair.as_rule(), Rule::function_arguments);
    let args = args_pair
        .into_inner()
        .map(parse_template_node)
        .try_collect()?;
    Ok(FunctionCallNode {
        name: name.as_str(),
        name_span: name.as_span(),
        args,
        args_span,
    })
}

fn parse_term_node(pair: Pair<Rule>) -> TemplateParseResult<ExpressionNode> {
    assert_eq!(pair.as_rule(), Rule::term);
    let mut inner = pair.into_inner();
    let expr = inner.next().unwrap();
    let span = expr.as_span();
    let primary = match expr.as_rule() {
        Rule::literal => {
            let text = parse_string_literal(expr);
            ExpressionNode::new(ExpressionKind::String(text), span)
        }
        Rule::integer_literal => {
            let value = expr.as_str().parse().map_err(|err| {
                TemplateParseError::with_span(TemplateParseErrorKind::ParseIntError(err), span)
            })?;
            ExpressionNode::new(ExpressionKind::Integer(value), span)
        }
        Rule::identifier => ExpressionNode::new(ExpressionKind::Identifier(expr.as_str()), span),
        Rule::function => {
            let function = parse_function_call_node(expr)?;
            ExpressionNode::new(ExpressionKind::FunctionCall(function), span)
        }
        Rule::template => parse_template_node(expr)?,
        other => panic!("unexpected term: {other:?}"),
    };
    inner.try_fold(primary, |object, chain| {
        assert_eq!(chain.as_rule(), Rule::function);
        let span = chain.as_span();
        let method = MethodCallNode {
            object: Box::new(object),
            function: parse_function_call_node(chain)?,
        };
        Ok(ExpressionNode::new(
            ExpressionKind::MethodCall(method),
            span,
        ))
    })
}

fn parse_template_node(pair: Pair<Rule>) -> TemplateParseResult<ExpressionNode> {
    assert_eq!(pair.as_rule(), Rule::template);
    let span = pair.as_span();
    let inner = pair.into_inner();
    let mut nodes: Vec<_> = inner.map(parse_term_node).try_collect()?;
    if nodes.len() == 1 {
        Ok(nodes.pop().unwrap())
    } else {
        Ok(ExpressionNode::new(ExpressionKind::List(nodes), span))
    }
}

/// Parses text into AST nodes. No type/name checking is made at this stage.
pub fn parse_template(template_text: &str) -> TemplateParseResult<ExpressionNode> {
    let mut pairs: Pairs<Rule> = TemplateParser::parse(Rule::program, template_text)?;
    let first_pair = pairs.next().unwrap();
    if first_pair.as_rule() == Rule::EOI {
        let span = first_pair.as_span();
        Ok(ExpressionNode::new(ExpressionKind::List(Vec::new()), span))
    } else {
        parse_template_node(first_pair)
    }
}

enum Property<'a, I> {
    String(Box<dyn TemplateProperty<I, Output = String> + 'a>),
    Boolean(Box<dyn TemplateProperty<I, Output = bool> + 'a>),
    Integer(Box<dyn TemplateProperty<I, Output = i64> + 'a>),
    CommitOrChangeId(Box<dyn TemplateProperty<I, Output = CommitOrChangeId<'a>> + 'a>),
    ShortestIdPrefix(Box<dyn TemplateProperty<I, Output = ShortestIdPrefix> + 'a>),
    Signature(Box<dyn TemplateProperty<I, Output = Signature> + 'a>),
    Timestamp(Box<dyn TemplateProperty<I, Output = Timestamp> + 'a>),
}

impl<'a, I: 'a> Property<'a, I> {
    fn try_into_boolean(self) -> Option<Box<dyn TemplateProperty<I, Output = bool> + 'a>> {
        match self {
            Property::String(property) => {
                Some(Box::new(TemplateFunction::new(property, |s| !s.is_empty())))
            }
            Property::Boolean(property) => Some(property),
            _ => None,
        }
    }

    fn try_into_integer(self) -> Option<Box<dyn TemplateProperty<I, Output = i64> + 'a>> {
        match self {
            Property::Integer(property) => Some(property),
            _ => None,
        }
    }

    fn into_plain_text(self) -> Box<dyn TemplateProperty<I, Output = String> + 'a> {
        match self {
            Property::String(property) => property,
            _ => Box::new(PlainTextFormattedProperty::new(self.into_template())),
        }
    }

    fn into_template(self) -> Box<dyn Template<I> + 'a> {
        fn wrap<'a, I: 'a, O: Template<()> + 'a>(
            property: Box<dyn TemplateProperty<I, Output = O> + 'a>,
        ) -> Box<dyn Template<I> + 'a> {
            Box::new(FormattablePropertyTemplate::new(property))
        }
        match self {
            Property::String(property) => wrap(property),
            Property::Boolean(property) => wrap(property),
            Property::Integer(property) => wrap(property),
            Property::CommitOrChangeId(property) => wrap(property),
            Property::ShortestIdPrefix(property) => wrap(property),
            Property::Signature(property) => wrap(property),
            Property::Timestamp(property) => wrap(property),
        }
    }
}

struct PropertyAndLabels<'a, C>(Property<'a, C>, Vec<String>);

impl<'a, C: 'a> PropertyAndLabels<'a, C> {
    fn into_template(self) -> Box<dyn Template<C> + 'a> {
        let PropertyAndLabels(property, labels) = self;
        if labels.is_empty() {
            property.into_template()
        } else {
            Box::new(LabelTemplate::new(
                property.into_template(),
                Literal(labels),
            ))
        }
    }
}

enum Expression<'a, C> {
    Property(PropertyAndLabels<'a, C>),
    Template(Box<dyn Template<C> + 'a>),
}

impl<'a, C: 'a> Expression<'a, C> {
    fn try_into_boolean(self) -> Option<Box<dyn TemplateProperty<C, Output = bool> + 'a>> {
        match self {
            Expression::Property(PropertyAndLabels(property, _)) => property.try_into_boolean(),
            Expression::Template(_) => None,
        }
    }

    fn try_into_integer(self) -> Option<Box<dyn TemplateProperty<C, Output = i64> + 'a>> {
        match self {
            Expression::Property(PropertyAndLabels(property, _)) => property.try_into_integer(),
            Expression::Template(_) => None,
        }
    }

    fn into_plain_text(self) -> Box<dyn TemplateProperty<C, Output = String> + 'a> {
        match self {
            Expression::Property(PropertyAndLabels(property, _)) => property.into_plain_text(),
            Expression::Template(template) => Box::new(PlainTextFormattedProperty::new(template)),
        }
    }

    fn into_template(self) -> Box<dyn Template<C> + 'a> {
        match self {
            Expression::Property(property_labels) => property_labels.into_template(),
            Expression::Template(template) => template,
        }
    }
}

fn expect_no_arguments(function: &FunctionCallNode) -> TemplateParseResult<()> {
    if function.args.is_empty() {
        Ok(())
    } else {
        Err(TemplateParseError::invalid_argument_count_exact(
            0,
            function.args_span,
        ))
    }
}

/// Extracts exactly N required arguments.
fn expect_exact_arguments<'a, 'i, const N: usize>(
    function: &'a FunctionCallNode<'i>,
) -> TemplateParseResult<&'a [ExpressionNode<'i>; N]> {
    function
        .args
        .as_slice()
        .try_into()
        .map_err(|_| TemplateParseError::invalid_argument_count_exact(N, function.args_span))
}

/// Extracts N required arguments and remainders.
fn expect_some_arguments<'a, 'i, const N: usize>(
    function: &'a FunctionCallNode<'i>,
) -> TemplateParseResult<(&'a [ExpressionNode<'i>; N], &'a [ExpressionNode<'i>])> {
    if function.args.len() >= N {
        let (required, rest) = function.args.split_at(N);
        Ok((required.try_into().unwrap(), rest))
    } else {
        Err(TemplateParseError::invalid_argument_count_range_from(
            N..,
            function.args_span,
        ))
    }
}

/// Extracts N required arguments and M optional arguments.
fn expect_arguments<'a, 'i, const N: usize, const M: usize>(
    function: &'a FunctionCallNode<'i>,
) -> TemplateParseResult<(
    &'a [ExpressionNode<'i>; N],
    [Option<&'a ExpressionNode<'i>>; M],
)> {
    let count_range = N..=(N + M);
    if count_range.contains(&function.args.len()) {
        let (required, rest) = function.args.split_at(N);
        let mut optional = rest.iter().map(Some).collect_vec();
        optional.resize(M, None);
        Ok((required.try_into().unwrap(), optional.try_into().unwrap()))
    } else {
        Err(TemplateParseError::invalid_argument_count_range(
            count_range,
            function.args_span,
        ))
    }
}

fn split_email(email: &str) -> (&str, Option<&str>) {
    if let Some((username, rest)) = email.split_once('@') {
        (username, Some(rest))
    } else {
        (email, None)
    }
}

fn build_method_call<'a, I: 'a>(
    method: &MethodCallNode,
    build_keyword: &impl Fn(&str, pest::Span) -> TemplateParseResult<PropertyAndLabels<'a, I>>,
) -> TemplateParseResult<Expression<'a, I>> {
    match build_expression(&method.object, build_keyword)? {
        Expression::Property(PropertyAndLabels(property, mut labels)) => {
            let property = match property {
                Property::String(property) => {
                    build_string_method(property, &method.function, build_keyword)?
                }
                Property::Boolean(property) => {
                    build_boolean_method(property, &method.function, build_keyword)?
                }
                Property::Integer(property) => {
                    build_integer_method(property, &method.function, build_keyword)?
                }
                Property::CommitOrChangeId(property) => {
                    build_commit_or_change_id_method(property, &method.function, build_keyword)?
                }
                Property::ShortestIdPrefix(property) => {
                    build_shortest_id_prefix_method(property, &method.function, build_keyword)?
                }
                Property::Signature(property) => {
                    build_signature_method(property, &method.function, build_keyword)?
                }
                Property::Timestamp(property) => {
                    build_timestamp_method(property, &method.function, build_keyword)?
                }
            };
            labels.push(method.function.name.to_owned());
            Ok(Expression::Property(PropertyAndLabels(property, labels)))
        }
        Expression::Template(_) => Err(TemplateParseError::no_such_method(
            "Template",
            &method.function,
        )),
    }
}

fn chain_properties<'a, I: 'a, J: 'a, O: 'a>(
    first: impl TemplateProperty<I, Output = J> + 'a,
    second: impl TemplateProperty<J, Output = O> + 'a,
) -> Box<dyn TemplateProperty<I, Output = O> + 'a> {
    Box::new(TemplateFunction::new(first, move |value| {
        second.extract(&value)
    }))
}

fn build_string_method<'a, I: 'a>(
    self_property: impl TemplateProperty<I, Output = String> + 'a,
    function: &FunctionCallNode,
    build_keyword: &impl Fn(&str, pest::Span) -> TemplateParseResult<PropertyAndLabels<'a, I>>,
) -> TemplateParseResult<Property<'a, I>> {
    let property = match function.name {
        "contains" => {
            let [needle_node] = expect_exact_arguments(function)?;
            // TODO: or .try_into_string() to disable implicit type cast?
            let needle_property = build_expression(needle_node, build_keyword)?.into_plain_text();
            Property::Boolean(chain_properties(
                (self_property, needle_property),
                TemplatePropertyFn(|(haystack, needle): &(String, String)| {
                    haystack.contains(needle)
                }),
            ))
        }
        "first_line" => {
            expect_no_arguments(function)?;
            Property::String(chain_properties(
                self_property,
                TemplatePropertyFn(|s: &String| s.lines().next().unwrap_or_default().to_string()),
            ))
        }
        _ => return Err(TemplateParseError::no_such_method("String", function)),
    };
    Ok(property)
}

fn build_boolean_method<'a, I: 'a>(
    _self_property: impl TemplateProperty<I, Output = bool> + 'a,
    function: &FunctionCallNode,
    _build_keyword: &impl Fn(&str, pest::Span) -> TemplateParseResult<PropertyAndLabels<'a, I>>,
) -> TemplateParseResult<Property<'a, I>> {
    Err(TemplateParseError::no_such_method("Boolean", function))
}

fn build_integer_method<'a, I: 'a>(
    _self_property: impl TemplateProperty<I, Output = i64> + 'a,
    function: &FunctionCallNode,
    _build_keyword: &impl Fn(&str, pest::Span) -> TemplateParseResult<PropertyAndLabels<'a, I>>,
) -> TemplateParseResult<Property<'a, I>> {
    Err(TemplateParseError::no_such_method("Integer", function))
}

fn build_commit_or_change_id_method<'a, I: 'a>(
    self_property: impl TemplateProperty<I, Output = CommitOrChangeId<'a>> + 'a,
    function: &FunctionCallNode,
    build_keyword: &impl Fn(&str, pest::Span) -> TemplateParseResult<PropertyAndLabels<'a, I>>,
) -> TemplateParseResult<Property<'a, I>> {
    let parse_optional_integer = |function| -> Result<Option<_>, TemplateParseError> {
        let ([], [len_node]) = expect_arguments(function)?;
        len_node
            .map(|node| {
                build_expression(node, build_keyword).and_then(|p| {
                    p.try_into_integer().ok_or_else(|| {
                        TemplateParseError::invalid_argument_type("Integer", node.span)
                    })
                })
            })
            .transpose()
    };
    let property = match function.name {
        "short" => {
            let len_property = parse_optional_integer(function)?;
            Property::String(chain_properties(
                (self_property, len_property),
                TemplatePropertyFn(|(id, len): &(CommitOrChangeId, Option<i64>)| {
                    id.short(len.and_then(|l| l.try_into().ok()).unwrap_or(12))
                }),
            ))
        }
        "shortest" => {
            let len_property = parse_optional_integer(function)?;
            Property::ShortestIdPrefix(chain_properties(
                (self_property, len_property),
                TemplatePropertyFn(|(id, len): &(CommitOrChangeId, Option<i64>)| {
                    id.shortest(len.and_then(|l| l.try_into().ok()).unwrap_or(0))
                }),
            ))
        }
        _ => {
            return Err(TemplateParseError::no_such_method(
                "CommitOrChangeId",
                function,
            ))
        }
    };
    Ok(property)
}

fn build_shortest_id_prefix_method<'a, I: 'a>(
    self_property: impl TemplateProperty<I, Output = ShortestIdPrefix> + 'a,
    function: &FunctionCallNode,
    _build_keyword: &impl Fn(&str, pest::Span) -> TemplateParseResult<PropertyAndLabels<'a, I>>,
) -> TemplateParseResult<Property<'a, I>> {
    let property = match function.name {
        "with_brackets" => {
            // TODO: If we had a map function, this could be expressed as a template
            // like 'id.shortest() % (.prefix() if(.rest(), "[" .rest() "]"))'
            expect_no_arguments(function)?;
            Property::String(chain_properties(
                self_property,
                TemplatePropertyFn(|id: &ShortestIdPrefix| id.with_brackets()),
            ))
        }
        _ => {
            return Err(TemplateParseError::no_such_method(
                "ShortestIdPrefix",
                function,
            ))
        }
    };
    Ok(property)
}

fn build_signature_method<'a, I: 'a>(
    self_property: impl TemplateProperty<I, Output = Signature> + 'a,
    function: &FunctionCallNode,
    _build_keyword: &impl Fn(&str, pest::Span) -> TemplateParseResult<PropertyAndLabels<'a, I>>,
) -> TemplateParseResult<Property<'a, I>> {
    let property = match function.name {
        "name" => {
            expect_no_arguments(function)?;
            Property::String(chain_properties(
                self_property,
                TemplatePropertyFn(|signature: &Signature| signature.name.clone()),
            ))
        }
        "email" => {
            expect_no_arguments(function)?;
            Property::String(chain_properties(
                self_property,
                TemplatePropertyFn(|signature: &Signature| signature.email.clone()),
            ))
        }
        "username" => {
            expect_no_arguments(function)?;
            Property::String(chain_properties(
                self_property,
                TemplatePropertyFn(|signature: &Signature| {
                    let (username, _) = split_email(&signature.email);
                    username.to_owned()
                }),
            ))
        }
        "timestamp" => {
            expect_no_arguments(function)?;
            Property::Timestamp(chain_properties(
                self_property,
                TemplatePropertyFn(|signature: &Signature| signature.timestamp.clone()),
            ))
        }
        _ => return Err(TemplateParseError::no_such_method("Signature", function)),
    };
    Ok(property)
}

fn build_timestamp_method<'a, I: 'a>(
    self_property: impl TemplateProperty<I, Output = Timestamp> + 'a,
    function: &FunctionCallNode,
    _build_keyword: &impl Fn(&str, pest::Span) -> TemplateParseResult<PropertyAndLabels<'a, I>>,
) -> TemplateParseResult<Property<'a, I>> {
    let property = match function.name {
        "ago" => {
            expect_no_arguments(function)?;
            Property::String(chain_properties(
                self_property,
                TemplatePropertyFn(time_util::format_timestamp_relative_to_now),
            ))
        }
        _ => return Err(TemplateParseError::no_such_method("Timestamp", function)),
    };
    Ok(property)
}

fn build_global_function<'a, C: 'a>(
    function: &FunctionCallNode,
    build_keyword: &impl Fn(&str, pest::Span) -> TemplateParseResult<PropertyAndLabels<'a, C>>,
) -> TemplateParseResult<Expression<'a, C>> {
    let expression = match function.name {
        "label" => {
            let [label_node, content_node] = expect_exact_arguments(function)?;
            let label_property = build_expression(label_node, build_keyword)?.into_plain_text();
            let content = build_expression(content_node, build_keyword)?.into_template();
            let labels = TemplateFunction::new(label_property, |s| {
                s.split_whitespace().map(ToString::to_string).collect()
            });
            let template = Box::new(LabelTemplate::new(content, labels));
            Expression::Template(template)
        }
        "if" => {
            let ([condition_node, true_node], [false_node]) = expect_arguments(function)?;
            let condition = build_expression(condition_node, build_keyword)?
                .try_into_boolean()
                .ok_or_else(|| {
                    TemplateParseError::invalid_argument_type("Boolean", condition_node.span)
                })?;
            let true_template = build_expression(true_node, build_keyword)?.into_template();
            let false_template = false_node
                .map(|node| build_expression(node, build_keyword))
                .transpose()?
                .map(|x| x.into_template());
            let template = Box::new(ConditionalTemplate::new(
                condition,
                true_template,
                false_template,
            ));
            Expression::Template(template)
        }
        "separate" => {
            let ([separator_node], content_nodes) = expect_some_arguments(function)?;
            let separator = build_expression(separator_node, build_keyword)?.into_template();
            let contents = content_nodes
                .iter()
                .map(|node| build_expression(node, build_keyword).map(|x| x.into_template()))
                .try_collect()?;
            let template = Box::new(SeparateTemplate::new(separator, contents));
            Expression::Template(template)
        }
        _ => return Err(TemplateParseError::no_such_function(function)),
    };
    Ok(expression)
}

fn build_commit_keyword<'a>(
    repo: RepoRef<'a>,
    workspace_id: &WorkspaceId,
    name: &str,
    span: pest::Span,
) -> TemplateParseResult<PropertyAndLabels<'a, Commit>> {
    fn wrap_fn<'a, O>(
        f: impl Fn(&Commit) -> O + 'a,
    ) -> Box<dyn TemplateProperty<Commit, Output = O> + 'a> {
        Box::new(TemplatePropertyFn(f))
    }
    let property = match name {
        "description" => Property::String(wrap_fn(|commit| {
            cli_util::complete_newline(commit.description())
        })),
        "change_id" => Property::CommitOrChangeId(wrap_fn(move |commit| {
            CommitOrChangeId::new(repo, commit.change_id())
        })),
        "commit_id" => Property::CommitOrChangeId(wrap_fn(move |commit| {
            CommitOrChangeId::new(repo, commit.id())
        })),
        "author" => Property::Signature(wrap_fn(|commit| commit.author().clone())),
        "committer" => Property::Signature(wrap_fn(|commit| commit.committer().clone())),
        "working_copies" => Property::String(Box::new(WorkingCopiesProperty { repo })),
        "current_working_copy" => {
            let workspace_id = workspace_id.clone();
            Property::Boolean(wrap_fn(move |commit| {
                Some(commit.id()) == repo.view().get_wc_commit_id(&workspace_id)
            }))
        }
        "branches" => Property::String(Box::new(BranchProperty { repo })),
        "tags" => Property::String(Box::new(TagProperty { repo })),
        "git_refs" => Property::String(Box::new(GitRefsProperty { repo })),
        "git_head" => Property::String(Box::new(GitHeadProperty::new(repo))),
        "divergent" => Property::Boolean(wrap_fn(move |commit| {
            // The given commit could be hidden in e.g. obslog.
            let maybe_entries = repo.resolve_change_id(commit.change_id());
            maybe_entries.map_or(0, |entries| entries.len()) > 1
        })),
        "conflict" => Property::Boolean(wrap_fn(|commit| commit.tree().has_conflict())),
        "empty" => Property::Boolean(wrap_fn(move |commit| {
            commit.tree().id() == rewrite::merge_commit_trees(repo, &commit.parents()).id()
        })),
        _ => return Err(TemplateParseError::no_such_keyword(name, span)),
    };
    Ok(PropertyAndLabels(property, vec![name.to_owned()]))
}

/// Builds template evaluation tree from AST nodes.
fn build_expression<'a, C: 'a>(
    node: &ExpressionNode,
    build_keyword: &impl Fn(&str, pest::Span) -> TemplateParseResult<PropertyAndLabels<'a, C>>,
) -> TemplateParseResult<Expression<'a, C>> {
    match &node.kind {
        ExpressionKind::Identifier(name) => {
            Ok(Expression::Property(build_keyword(name, node.span)?))
        }
        ExpressionKind::Integer(value) => {
            let term = PropertyAndLabels(Property::Integer(Box::new(Literal(*value))), vec![]);
            Ok(Expression::Property(term))
        }
        ExpressionKind::String(value) => {
            let term =
                PropertyAndLabels(Property::String(Box::new(Literal(value.clone()))), vec![]);
            Ok(Expression::Property(term))
        }
        ExpressionKind::List(nodes) => {
            let templates = nodes
                .iter()
                .map(|node| build_expression(node, build_keyword).map(|x| x.into_template()))
                .try_collect()?;
            Ok(Expression::Template(Box::new(ListTemplate(templates))))
        }
        ExpressionKind::FunctionCall(function) => build_global_function(function, build_keyword),
        ExpressionKind::MethodCall(method) => build_method_call(method, build_keyword),
    }
}

// TODO: We'll probably need a trait that abstracts the Property enum and
// keyword/method parsing functions per the top-level context.
pub fn parse_commit_template<'a>(
    repo: RepoRef<'a>,
    workspace_id: &WorkspaceId,
    template_text: &str,
) -> TemplateParseResult<Box<dyn Template<Commit> + 'a>> {
    let node = parse_template(template_text)?;
    let expression = build_expression(&node, &|name, span| {
        build_commit_keyword(repo, workspace_id, name, span)
    })?;
    Ok(expression.into_template())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(template_text: &str) -> TemplateParseResult<Expression<()>> {
        let node = parse_template(template_text)?;
        build_expression(&node, &|name, span| {
            Err(TemplateParseError::no_such_keyword(name, span))
        })
    }

    /// Drops auxiliary data of AST so it can be compared with other node.
    fn normalize_tree(node: ExpressionNode) -> ExpressionNode {
        fn empty_span() -> pest::Span<'static> {
            pest::Span::new("", 0, 0).unwrap()
        }

        fn normalize_list(nodes: Vec<ExpressionNode>) -> Vec<ExpressionNode> {
            nodes.into_iter().map(normalize_tree).collect()
        }

        fn normalize_function_call(function: FunctionCallNode) -> FunctionCallNode {
            FunctionCallNode {
                name: function.name,
                name_span: empty_span(),
                args: normalize_list(function.args),
                args_span: empty_span(),
            }
        }

        let normalized_kind = match node.kind {
            ExpressionKind::Identifier(_)
            | ExpressionKind::Integer(_)
            | ExpressionKind::String(_) => node.kind,
            ExpressionKind::List(nodes) => ExpressionKind::List(normalize_list(nodes)),
            ExpressionKind::FunctionCall(function) => {
                ExpressionKind::FunctionCall(normalize_function_call(function))
            }
            ExpressionKind::MethodCall(method) => {
                let object = Box::new(normalize_tree(*method.object));
                let function = normalize_function_call(method.function);
                ExpressionKind::MethodCall(MethodCallNode { object, function })
            }
        };
        ExpressionNode {
            kind: normalized_kind,
            span: empty_span(),
        }
    }

    #[test]
    fn test_parse_tree_eq() {
        assert_eq!(
            normalize_tree(parse_template(r#" commit_id.short(1 )  description"#).unwrap()),
            normalize_tree(parse_template(r#"commit_id.short( 1 ) (description)"#).unwrap()),
        );
        assert_ne!(
            normalize_tree(parse_template(r#" "ab" "#).unwrap()),
            normalize_tree(parse_template(r#" "a" "b" "#).unwrap()),
        );
        assert_ne!(
            normalize_tree(parse_template(r#" "foo" "0" "#).unwrap()),
            normalize_tree(parse_template(r#" "foo" 0 "#).unwrap()),
        );
    }

    #[test]
    fn test_function_call_syntax() {
        // Trailing comma isn't allowed for empty argument
        assert!(parse(r#" "".first_line() "#).is_ok());
        assert!(parse(r#" "".first_line(,) "#).is_err());

        // Trailing comma is allowed for the last argument
        assert!(parse(r#" "".contains("") "#).is_ok());
        assert!(parse(r#" "".contains("",) "#).is_ok());
        assert!(parse(r#" "".contains("" ,  ) "#).is_ok());
        assert!(parse(r#" "".contains(,"") "#).is_err());
        assert!(parse(r#" "".contains("",,) "#).is_err());
        assert!(parse(r#" "".contains("" , , ) "#).is_err());
        assert!(parse(r#" label("","") "#).is_ok());
        assert!(parse(r#" label("","",) "#).is_ok());
        assert!(parse(r#" label("",,"") "#).is_err());
    }

    #[test]
    fn test_integer_literal() {
        let extract = |x: Expression<()>| x.try_into_integer().unwrap().extract(&());

        assert_eq!(extract(parse("0").unwrap()), 0);
        assert_eq!(extract(parse("(42)").unwrap()), 42);
        assert!(parse("00").is_err());

        assert_eq!(extract(parse(&format!("{}", i64::MAX)).unwrap()), i64::MAX);
        assert!(parse(&format!("{}", (i64::MAX as u64) + 1)).is_err());
    }
}
