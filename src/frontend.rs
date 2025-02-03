/*
 * Copyright (c) 2023-2025 Atsushi Komaba
 *
 * This file is part of Syscraws.
 * Syscraws is free software: you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation, either version 3
 * of the License, or any later version.
 *
 * Syscraws is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with Syscraws. If not, see <https://www.gnu.org/licenses/>.
 */

mod chars_peekable;
mod tests;

use enum_iterator::Sequence;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::backend;
use crate::log::{self, Index, ParseError, Pos};
use chars_peekable::CharsPeekable;

/**
 * Reads the file `root_file_path` and all other files it imports,
 * and passes them to `backend`.
 */
pub fn read_input(root_file_path: &Path) {
    let root_file_path = root_file_path.with_extension("sysc");
    let root_file_path = match root_file_path.canonicalize() {
        Ok(path) => path,
        Err(err) => {
            log::root_file_not_found(&root_file_path, err);
            return;
        }
    };
    let mut reader = Reader {
        files: Vec::new(),
        file_indices: HashMap::new(),
        import_chain: HashSet::from([root_file_path.clone()]),
        function_definitions: Vec::new(),
        items: Vec::new(),
        num_errors: 0,
    };
    if let Err(err) = reader.read_file(&root_file_path) {
        log::cannot_read_root_file(&root_file_path, err);
        reader.num_errors += 1;
    }
    if reader.num_errors > 0 {
        log::abort(reader.num_errors);
        return;
    }

    let mut num_global_variables = 0;
    for (file_index, (path, _, _, stmts)) in reader.files.iter().enumerate() {
        println!("{}", path.display());
        let mut global_variables = HashMap::new();
        let mut variables_in_global_scope = Vec::new();
        for stmt in stmts {
            translate_stmt(
                stmt,
                &mut global_variables,
                &mut num_global_variables,
                &mut variables_in_global_scope,
                path,
                &reader.files,
                file_index,
                &reader.items,
            );
        }
        for (name, index) in global_variables {
            reader.items[file_index].insert(name, Item::GlobalVariable(index));
        }
    }
    for (i, definition) in reader.function_definitions.iter().enumerate() {
        let file_index = reader.file_indices[&definition.path];
        let mut num_local_variables = 0;
        let mut local_variables = HashMap::new();
        let mut variables_in_scope = Vec::new();
        for stmt in &definition.body {
            translate_stmt(
                stmt,
                &mut local_variables,
                &mut num_local_variables,
                &mut variables_in_scope,
                &definition.path,
                &reader.files,
                file_index,
                &reader.items,
            );
        }
    }
}

fn translate_stmt(
    stmt: &Stmt,
    variables: &mut HashMap<String, usize>,
    num_variables: &mut usize,
    variables_in_scope: &mut Vec<(String, Option<usize>)>,
    path: &Path,
    files: &Vec<(PathBuf, String, Vec<Range<usize>>, Vec<Stmt>)>,
    file_index: usize,
    items: &Vec<HashMap<String, Item>>,
) -> backend::Stmt {
    match stmt {
        Stmt::Var(name) => {
            let Term::Identifier(ref name) = name.term else {
                panic!("Invalid variable name");
            };
            let new_index = *num_variables;
            let prev_index = variables.insert(name.clone(), new_index);
            variables_in_scope.push((name.clone(), prev_index));
            *num_variables += 1;
            backend::Stmt::Expr(backend::Expr::GlobalVariable(new_index))
        }
        Stmt::Term(term) => backend::Stmt::Expr(translate_expr(
            term, path, files, variables, file_index, items,
        )),
        Stmt::While(condition, body) => {
            let condition = translate_expr(condition, path, files, variables, file_index, items);
            let mut variables_in_body = Vec::new();
            let body = body
                .iter()
                .map(|body_stmt| {
                    translate_stmt(
                        body_stmt,
                        variables,
                        num_variables,
                        &mut variables_in_body,
                        path,
                        files,
                        file_index,
                        items,
                    )
                })
                .collect();
            backend::Stmt::While(condition, body)
        }
    }
}

fn translate_expr(
    term: &TermWithPos,
    path: &Path,
    files: &Vec<(PathBuf, String, Vec<Range<usize>>, Vec<Stmt>)>,
    local_variables: &HashMap<String, usize>,
    file_index: usize,
    items: &Vec<HashMap<String, Item>>,
) -> backend::Expr {
    match &term.term {
        Term::Identifier(name) => {
            if let Some(n) = local_variables.get(name) {
                return backend::Expr::LocalVariable(*n);
            }
            if let Some(item) = &items[file_index].get(name) {
                return match item {
                    Item::Function(n) => backend::Expr::Function(n.clone()),
                    Item::GlobalVariable(n) => backend::Expr::GlobalVariable(*n),
                    Item::Import(n) => backend::Expr::Module(*n),
                    Item::Type(_) => todo!(),
                };
            }
            panic!("Undefined variable");
        }
        _ => todo!(),
    }
}

struct Reader {
    files: Vec<(PathBuf, String, Vec<Range<usize>>, Vec<Stmt>)>,
    items: Vec<HashMap<String, Item>>,
    function_definitions: Vec<FunctionDefinition>,
    file_indices: HashMap<PathBuf, usize>,
    import_chain: HashSet<PathBuf>,
    num_errors: u32,
}

impl Reader {
    /**
     * Reads a file specified by the first argument `path`
     * and appends it to `files` and `file_indices`.
     *
     * `read_file` calls itself rucursively if the file imports another file.
     *
     * - `files`: the result are stored.
     * - `file_indices`: used to prevent a file to be read more than once.
     *
     * The index obtained through `file_indices` is equal to the index in `files`
     * after `read_file` is called with no errors.
     *
     * - `imported_from`: used to detect and report circular imports.
     *
     * It calls `parse_file` to handle various parse errors at once.
     */
    fn read_file(&mut self, path: &Path) -> Result<usize, std::io::Error> {
        if let Some(&index) = self.file_indices.get(path) {
            // The file specified by `path` was already read.
            // Since circular imports should have been detected in `parse_imports`,
            // this is not circular imports but diamond imports.
            return Ok(index);
        }
        let mut file = std::fs::File::open(path)?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        let mut chars_peekable = CharsPeekable::new(&content);
        match self.parse_file(&mut chars_peekable, path) {
            Ok((stmts, items)) => {
                let lines = chars_peekable.lines();
                self.files.push((path.to_path_buf(), content, lines, stmts));
                self.items.push(items);
            }
            Err(err) => {
                err.eprint(path, &content, &chars_peekable.lines());
                self.num_errors += 1;
            }
        };
        let new_index = self.file_indices.len();
        self.file_indices.insert(path.to_path_buf(), new_index);
        Ok(new_index)
    }

    /**
     *
     */
    fn parse_file(
        &mut self,
        chars_peekable: &mut CharsPeekable,
        path: &Path,
    ) -> Result<(Vec<Stmt>, HashMap<String, Item>), ParseError> {
        let mut parser = Parser::new(chars_peekable)?;
        let mut stmts = Vec::new();
        let mut items = HashMap::new();
        while let Some(item_start_token) = parser.next_token_mut() {
            if let Token::KeywordImport = item_start_token {
                let (name, index) = parser.parse_import(self, path.parent().unwrap())?;
                items.insert(name, Item::Import(index));
            } else if let Token::KeywordFunc = item_start_token {
                let (name, definition) = parser.parse_function_definition(path)?;

                match items
                    .entry(name)
                    .or_insert_with(|| Item::Function(Vec::new()))
                {
                    Item::Function(definitions) => {
                        let new_index = self.function_definitions.len();
                        self.function_definitions.push(definition);
                        definitions.push(new_index);
                    }
                    _ => return Err(ParseError::DuplicateDefinition),
                }
            } else if let Some(stmt) = parser.parse_stmt(&mut Vec::new())? {
                stmts.push(stmt);
            } else {
                return Err(ParseError::UnexpectedToken(parser.next_token_pos()));
            }
        }
        Ok((stmts, items))
    }
}

impl Parser<'_, '_> {
    fn parse_import(
        &mut self,
        reader: &mut Reader,
        parent_directory: &Path,
    ) -> Result<(String, usize), ParseError> {
        let keyword_import_pos = self.next_token_pos();
        self.consume_token()?;

        // An identifier should immediately follow `import`, without a line break.
        let import_name = match self.next_token_on_current_line_mut() {
            Some(Token::Identifier(name)) => std::mem::take(name),
            Some(_) => {
                let unexpected_token_pos = self.next_token_pos();
                return Err(ParseError::UnexpectedTokenAfterKeywordImport {
                    unexpected_token_pos,
                    keyword_import_pos,
                });
            }
            None => return Err(ParseError::MissingImportName { keyword_import_pos }),
        };
        let import_name_pos = self.next_token_pos();
        self.consume_token()?;

        // The path can be specified by the following string literal in parentheses.
        // Inferred from import_name if omitted.
        let import_path = match self.next_token_on_current_line_ref() {
            Some(Token::OpeningParenthesis) => {
                let opening_parenthesis_pos = self.next_token_pos();
                self.consume_token()?;

                let import_path = self.parse_assign(true)?;
                match self.next_token_ref() {
                    Some(Token::ClosingParenthesis) => {}
                    Some(_) => {
                        let unexpected_token_pos = self.next_token_pos();
                        return Err(ParseError::UnexpectedTokenInParentheses {
                            unexpected_token_pos,
                            opening_parenthesis_pos,
                        });
                    }
                    None => {
                        return Err(ParseError::UnclosedParenthesis {
                            opening_parenthesis_pos,
                        })
                    }
                }
                let closing_parenthesis_pos = self.next_token_pos();
                self.consume_token()?;

                let Some(import_path) = import_path else {
                    return Err(ParseError::MissingImportPath {
                        opening_parenthesis_pos,
                        closing_parenthesis_pos,
                    });
                };

                let Term::StringLiteral(mut import_path_components) = import_path.term else {
                    return Err(ParseError::InvalidImportPath {
                        term_pos: import_path.pos,
                    });
                };
                if import_path_components.len() != 1 {
                    return Err(ParseError::InvalidImportPath {
                        term_pos: import_path.pos,
                    });
                }
                let Some(StringLiteralComponent::String(import_path)) =
                    import_path_components.pop()
                else {
                    return Err(ParseError::InvalidImportPath {
                        term_pos: import_path.pos,
                    });
                };
                parent_directory.join(&import_path)
            }
            Some(_) => {
                let unexpected_token_pos = self.next_token_pos();
                return Err(ParseError::UnexpectedTokenAfterImportName {
                    import_name_pos,
                    unexpected_token_pos,
                });
            }
            None => parent_directory.join(&import_name),
        };

        let import_path = import_path.with_extension("sysc");
        let import_path = match import_path.canonicalize() {
            Ok(path) => path,
            Err(err) => {
                return Err(ParseError::CannotReadImportedFile {
                    path: import_path,
                    err,
                });
            }
        };
        if !reader.import_chain.insert(import_path.clone()) {
            return Err(ParseError::CircularImports { path: import_path });
        }
        match reader.read_file(&import_path) {
            Ok(n) => {
                reader.import_chain.remove(&import_path);
                Ok((import_name, n))
            }
            Err(err) => {
                return Err(ParseError::CannotReadImportedFile {
                    path: import_path,
                    err,
                });
            }
        }
    }

    fn parse_function_definition(
        &mut self,
        path: &Path,
    ) -> Result<(String, FunctionDefinition), ParseError> {
        let keyword_func_pos = self.next_token_pos();
        self.consume_token()?;

        // An identifier should immediately follow `import`, without a line break.
        let function_name = match self.next_token_on_current_line_mut() {
            Some(Token::Identifier(name)) => std::mem::take(name),
            Some(_) => {
                return Err(ParseError::UnexpectedTokenAfterKeywordFunc {
                    unexpected_token_pos: self.next_token_pos(),
                    keyword_func_pos,
                });
            }
            None => return Err(ParseError::MissingFunctionName { keyword_func_pos }),
        };
        self.consume_token()?;

        // Generic parameters list can follow.
        let opt_type_parameters =
            if let Some(Token::OpeningBracket) = self.next_token_on_current_line_ref() {
                todo!("Parse generic parameters");
            } else {
                None
            };

        // parameters list follows.
        let opt_parameters =
            if let Some(Token::OpeningParenthesis) = self.next_token_on_current_line_ref() {
                let opening_parenthesis_pos = self.next_token_pos();
                self.consume_token()?;

                let mut parameters = Vec::new();
                loop {
                    let parameter = self.parse_assign(true)?;
                    match self.next_token_ref() {
                        Some(Token::ClosingParenthesis) => {
                            self.consume_token()?;
                            if let Some(element) = parameter {
                                parameters.push(ListElement::NonEmpty(element));
                            }
                            break;
                        }
                        Some(Token::Comma) => {
                            let comma_pos = self.next_token_pos();
                            self.consume_token()?;
                            if let Some(element) = parameter {
                                parameters.push(ListElement::NonEmpty(element));
                            } else {
                                parameters.push(ListElement::Empty { comma_pos })
                            }
                        }
                        Some(_) => {
                            return Err(ParseError::UnexpectedTokenInParentheses {
                                unexpected_token_pos: self.next_token_pos(),
                                opening_parenthesis_pos,
                            });
                        }
                        None => {
                            return Err(ParseError::UnclosedParenthesis {
                                opening_parenthesis_pos,
                            });
                        }
                    }
                }
                Some(parameters)
            } else {
                None
            };

        // The return type can be written after `->` or `:` (undecided).
        let opt_ret_ty = if let Some(Token::HyphenGreater) = self.next_token_ref() {
            let arrow_pos = self.next_token_pos();
            self.consume_token()?;
            Some(RetTy {
                arrow_pos,
                opt_ret_ty: self.parse_disjunction(false)?,
            })
        } else {
            None
        };

        // The function body follows.
        let body = self.parse_block(&mut vec![keyword_func_pos.line()])?;

        Ok((
            function_name,
            FunctionDefinition {
                path: path.to_path_buf(),
                opt_parameters,
                opt_type_parameters,
                opt_ret_ty,
                body,
            },
        ))
    }

    /**
     * Parses a block consisting of statements and a keyword `end`.
     */
    fn parse_block(
        &mut self,
        start_line_indices: &mut Vec<usize>,
    ) -> Result<Vec<Stmt>, ParseError> {
        let mut stmts = Vec::new();
        loop {
            if let Some(Token::KeywordEnd) = self.next_token_ref() {
                self.consume_token()?;
                return Ok(stmts);
            } else if let Some(stmt) = self.parse_stmt(start_line_indices)? {
                stmts.push(stmt);
            } else if self.has_remaining_token() {
                return Err(ParseError::UnexpectedTokenInBlock {
                    unexpected_token_pos: self.next_token_pos(),
                    start_line_indices: std::mem::take(start_line_indices),
                });
            } else {
                return Err(ParseError::UnclosedBlock {
                    start_line_indices: std::mem::take(start_line_indices),
                });
            }
        }
    }

    fn parse_stmt(
        &mut self,
        start_line_indices: &mut Vec<usize>,
    ) -> Result<Option<Stmt>, ParseError> {
        if let Some(Token::KeywordVar) = self.next_token_ref() {
            let keyword_var_pos = self.next_token_pos();
            self.consume_token()?;
            let Some(term) = self.parse_assign(false)? else {
                panic!("No variable name after keyword `var` at {keyword_var_pos}");
            };
            Ok(Some(Stmt::Var(term)))
        } else if let Some(Token::KeywordWhile) = self.next_token_ref() {
            let keyword_while_pos = self.next_token_pos();
            self.consume_token()?;

            // The condition should immediately follow `while`, without line break.
            if !self.has_remaining_token_on_current_line() {
                return Err(ParseError::MissingConditionAfterKeywordWhile { keyword_while_pos });
            }
            let Some(condition) = self.parse_disjunction(false)? else {
                return Err(ParseError::UnexpectedTokenAfterKeywordWhile {
                    unexpected_token_pos: self.next_token_pos(),
                    keyword_while_pos,
                });
            };
            // A line break is required right after the condition.
            if self.has_remaining_token_on_current_line() {
                return Err(ParseError::UnexpectedTokenAfterWhileCondition {
                    unexpected_token_pos: self.next_token_pos(),
                    condition_pos: condition.pos,
                });
            }

            start_line_indices.push(keyword_while_pos.line());
            let body = self.parse_block(start_line_indices)?;
            start_line_indices.pop();
            Ok(Some(Stmt::While(condition, body)))
        } else if let Some(term) = self.parse_assign(false)? {
            // A term immediately followed by a line break can be a statement.
            if self.has_remaining_token_on_current_line() {
                return Err(ParseError::UnexpectedTokenAfterStatement {
                    unexpected_token_pos: self.next_token_pos(),
                    stmt_pos: term.pos,
                });
            }
            Ok(Some(Stmt::Term(term)))
        } else {
            Ok(None)
        }
    }

    fn parse_assign(&mut self, allow_line_break: bool) -> Result<Option<TermWithPos>, ParseError> {
        let start = self.next_token_start();
        let left_hand_side = self.parse_disjunction(allow_line_break)?;
        if let Some(operator) = self.next_token_ref().and_then(assignment_operator) {
            let operator_pos = self.next_token_pos();
            self.consume_token()?;
            let right_hand_side = self.parse_assign(allow_line_break)?;
            Ok(Some(TermWithPos {
                pos: self.range_from(start),
                term: Term::Assignment {
                    operator: Box::new(TermWithPos {
                        term: Term::MethodName(operator.to_string()),
                        pos: operator_pos,
                    }),
                    opt_left_hand_side: left_hand_side.map(Box::new),
                    opt_right_hand_side: right_hand_side.map(Box::new),
                },
            }))
        } else {
            Ok(left_hand_side)
        }
    }

    pub fn parse_disjunction(
        &mut self,
        allow_line_break: bool,
    ) -> Result<Option<TermWithPos>, ParseError> {
        let start = self.next_token_start();
        let term = self.parse_conjunction(allow_line_break)?;
        if let Some(Token::DoubleBar) = self.next_token_ref() {
            let mut conditions = vec![term];
            let mut operators_pos = Vec::new();
            while let Some(Token::DoubleBar) = self.next_token_ref() {
                operators_pos.push(self.next_token_pos());
                self.consume_token()?;
                conditions.push(self.parse_conjunction(allow_line_break)?);
            }
            Ok(Some(TermWithPos {
                term: Term::Disjunction {
                    opt_conditions: conditions,
                    operators_pos,
                },
                pos: self.range_from(start),
            }))
        } else {
            return Ok(term);
        }
    }

    fn parse_conjunction(
        &mut self,
        allow_line_break: bool,
    ) -> Result<Option<TermWithPos>, ParseError> {
        let start = self.next_token_start();
        let term = self.parse_binary_operator(allow_line_break)?;
        if let Some(Token::DoubleAmpersand) = self.next_token_ref() {
            let mut conditions = vec![term];
            let mut operators_pos = Vec::new();
            while let Some(Token::DoubleAmpersand) = self.next_token_ref() {
                operators_pos.push(self.next_token_pos());
                self.consume_token()?;
                conditions.push(self.parse_binary_operator(allow_line_break)?);
            }
            Ok(Some(TermWithPos {
                term: Term::Conjunction {
                    opt_conditions: conditions,
                    operators_pos,
                },
                pos: self.range_from(start),
            }))
        } else {
            return Ok(term);
        }
    }

    fn parse_binary_operator(
        &mut self,
        allow_line_break: bool,
    ) -> Result<Option<TermWithPos>, ParseError> {
        self.parse_binary_operator_rec(allow_line_break, Precedence::first())
    }

    fn parse_binary_operator_rec(
        &mut self,
        allow_line_break: bool,
        precedence: Option<Precedence>,
    ) -> Result<Option<TermWithPos>, ParseError> {
        let Some(precedence) = precedence else {
            return self.parse_factor(allow_line_break);
        };
        let start = self.next_token_start();
        let mut left_operand =
            self.parse_binary_operator_rec(allow_line_break, precedence.next())?;
        /*
        while let Some((preceding_whitespace, ref token)) = lexer.next_token {
            if !delimited && preceding_whitespace == PrecedingWhitespace::Vertical {
                break;
            } else if let Some(operator) = infix_operator(token, precedence) {
                let operator_pos = lexer.next_token_pos();
                lexer.consume_token()?;
                let right_operand = parse_binary_operator_rec(lexer, delimited, precedence.next())?;
                left_operand = Some(TermPos {
                    term: Term::BinaryOperation {
                        opt_left_operand: left_operand.map(Box::new),
                        operator: Box::new(TermPos {
                            term: Term::MethodName(operator.to_string()),
                            pos: operator_pos,
                        }),
                        opt_right_operand: right_operand.map(Box::new),
                    },
                    pos: lexer.range_from(start),
                });
            } else {
                break;
            }
        }
        */
        Ok(left_operand)
    }

    fn parse_factor(&mut self, allow_line_break: bool) -> Result<Option<TermWithPos>, ParseError> {
        let factor_start = self.next_token_start();
        let Some(first_token) = self.next_token_mut() else {
            return Ok(None);
        };
        let mut factor = if let Token::Underscore = first_token {
            Term::Identity
        } else if let Token::Identifier(ref mut name) = first_token {
            let name = std::mem::take(name);
            self.consume_token()?;
            Term::Identifier(name)
        } else if let Token::StringLiteral(ref mut components) = first_token {
            let components = std::mem::take(components);
            self.consume_token()?;
            Term::StringLiteral(components)
        } else if let Token::Digits(ref mut value) = first_token {
            let mut value = std::mem::take(value);
            self.consume_token()?;
            if let Some(Token::Dot) = self.adjacent_token_ref() {
                let number_pos = self.range_from(factor_start);
                self.consume_token()?;
                if let Some(Token::Identifier(ref mut name)) = self.next_token_mut() {
                    let number = TermWithPos {
                        term: Term::NumericLiteral(value),
                        pos: number_pos,
                    };
                    let name = std::mem::take(name);
                    self.consume_token()?;
                    Term::FieldByName {
                        term_left: Box::new(number),
                        name,
                    }
                } else {
                    value.push('.');
                    if let Some(Token::Digits(ref decimal_part)) = self.adjacent_token_ref() {
                        value.push_str(decimal_part);
                        self.consume_token()?;
                    }
                    Term::NumericLiteral(value)
                }
            } else {
                Term::NumericLiteral(value)
            }
        } else if let Token::Dot = first_token {
            let dot_pos = self.next_token_pos();
            self.consume_token()?;
            if let Some(Token::Digits(ref value)) = self.adjacent_token_ref() {
                let value = format!(".{value}");
                self.consume_token()?;
                Term::NumericLiteral(value)
            } else {
                return Err(ParseError::UnexpectedToken(dot_pos));
            }
        } else if let Token::OpeningParenthesis = first_token {
            let opening_parenthesis_pos = self.next_token_pos();
            self.consume_token()?;
            let mut elements = Vec::new();
            let has_trailing_comma;
            loop {
                let element = self.parse_assign(true)?;
                match self.next_token_ref() {
                    Some(Token::ClosingParenthesis) => {
                        self.consume_token()?;
                        if let Some(element) = element {
                            has_trailing_comma = false;
                            elements.push(ListElement::NonEmpty(element));
                        } else {
                            has_trailing_comma = true;
                        }
                        break;
                    }
                    Some(Token::Comma) => {
                        let comma_pos = self.next_token_pos();
                        self.consume_token()?;
                        if let Some(element) = element {
                            elements.push(ListElement::NonEmpty(element));
                        } else {
                            elements.push(ListElement::Empty { comma_pos })
                        }
                    }
                    Some(_) => {
                        return Err(ParseError::UnexpectedTokenInParentheses {
                            unexpected_token_pos: self.next_token_pos(),
                            opening_parenthesis_pos,
                        });
                    }
                    None => {
                        return Err(ParseError::UnclosedParenthesis {
                            opening_parenthesis_pos,
                        });
                    }
                }
            }
            if elements.len() == 1 && !has_trailing_comma {
                let Some(ListElement::NonEmpty(element)) = elements.pop() else {
                    panic!();
                };
                Term::Parenthesized {
                    inner: Box::new(element),
                }
            } else {
                Term::Tuple { elements }
            }
        } else if let Some(operator) = prefix_operator(&first_token) {
            let operator_pos = self.next_token_pos();
            self.consume_token()?;
            let opt_operand = self.parse_factor(allow_line_break)?;
            Term::UnaryOperation {
                opt_operand: opt_operand.map(Box::new),
                operator: Box::new(TermWithPos {
                    term: Term::MethodName(operator.to_string()),
                    pos: operator_pos,
                }),
            }
        } else {
            return Ok(None);
        };
        let mut factor_pos = self.range_from(factor_start);
        /*
            while let Some((preceding_whitespace, ref token)) = lexer.next_token {
                if let Token::Dot = token {
                    let dot_pos = lexer.next_token_pos();
                    lexer.consume_token()?;
                    if let Some(Token::Identifier(ref mut name)) = lexer.next_token_mut() {
                        let name = std::mem::take(name);
                        lexer.consume_token()?;
                        factor = Term::FieldByName {
                            term_left: Box::new(TermWithPos {
                                term: factor,
                                pos: factor_pos,
                            }),
                            name,
                        };
                        factor_pos = lexer.range_from(factor_start);
                    } else if let Some((_, Token::Digits(ref mut number))) = lexer.next_token {
                        let number = std::mem::take(number);
                        lexer.consume_token()?;
                        factor = Term::FieldByNumber {
                            term_left: Box::new(TermWithPos {
                                term: factor,
                                pos: factor_pos,
                            }),
                            number,
                        };
                        factor_pos = lexer.range_from(factor_start);
                    } else {
                        panic!();
                    }
                } else if let Token::Colon = token {
                    let colon_pos = lexer.next_token_pos();
                    lexer.consume_token()?;
                    let opt_term_right = parse_factor(lexer, delimited)?;
                    factor = Term::TypeAnnotation {
                        term_left: Box::new(TermPos {
                            term: factor,
                            pos: factor_pos,
                        }),
                        colon_pos,
                        opt_term_right: opt_term_right.map(Box::new),
                    };
                    factor_pos = lexer.range_from(factor_start);
                } else if !delimited && preceding_whitespace == PrecedingWhitespace::Vertical {
                    break;
                } else if let Token::HyphenGreater = token {
                    let arrow_pos = lexer.next_token_pos();
                    lexer.consume_token()?;
                    let opt_ret = parse_factor(lexer, delimited)?;
                    factor = Term::ReturnType {
                        arrow_pos,
                        args: Box::new(TermPos {
                            term: factor,
                            pos: factor_pos,
                        }),
                        opt_ret: opt_ret.map(Box::new),
                    };
                    factor_pos = lexer.range_from(factor_start);
                } else if let Token::OpeningParenthesis = token {
                    let opening_parenthesis_pos = lexer.next_token_pos();
                    lexer.consume_token()?;
                    let mut elements = Vec::new();
                    loop {
                        let element = parse_assign(lexer, true)?;
                        match lexer.next_token {
                            Some((_, Token::ClosingParenthesis)) => {
                                lexer.consume_token()?;
                                if let Some(element) = element {
                                    elements.push(ListElement::NonEmpty(element));
                                }
                                break;
                            }
                            Some((_, Token::Comma)) => {
                                let comma_pos = lexer.next_token_pos();
                                lexer.consume_token()?;
                                if let Some(element) = element {
                                    elements.push(ListElement::NonEmpty(element));
                                } else {
                                    elements.push(ListElement::Empty { comma_pos })
                                }
                            }
                            Some(_) => {
                                return Err(ParseError::UnexpectedTokenInParentheses {
                                    unexpected_token_pos: lexer.next_token_pos(),
                                    opening_parenthesis_pos,
                                });
                            }
                            None => {
                                return Err(ParseError::UnclosedParenthesis {
                                    opening_parenthesis_pos,
                                });
                            }
                        }
                    }
                    factor = Term::FunctionCall {
                        function: Box::new(TermPos {
                            term: factor,
                            pos: factor_pos,
                        }),
                        arguments: elements,
                    };
                    factor_pos = lexer.range_from(factor_start);
                } else if let Token::OpeningBracket = token {
                    let opening_bracket_pos = lexer.next_token_pos();
                    lexer.consume_token()?;
                    let mut elements = Vec::new();
                    loop {
                        let element = parse_assign(lexer, true)?;
                        match lexer.next_token {
                            Some((_, Token::ClosingBracket)) => {
                                lexer.consume_token()?;
                                if let Some(element) = element {
                                    elements.push(ListElement::NonEmpty(element));
                                }
                                break;
                            }
                            Some((_, Token::Comma)) => {
                                let comma_pos = lexer.next_token_pos();
                                lexer.consume_token()?;
                                if let Some(element) = element {
                                    elements.push(ListElement::NonEmpty(element));
                                } else {
                                    elements.push(ListElement::Empty { comma_pos })
                                }
                            }
                            Some(_) => {
                                return Err(ParseError::UnexpectedTokenInBrackets {
                                    unexpected_token_pos: lexer.next_token_pos(),
                                    opening_bracket_pos,
                                });
                            }
                            None => {
                                return Err(ParseError::UnclosedBracket {
                                    opening_bracket_pos,
                                });
                            }
                        }
                    }
                    factor = Term::TypeParameters {
                        term_left: Box::new(TermPos {
                            term: factor,
                            pos: factor_pos,
                        }),
                        parameters: elements,
                    };
                    factor_pos = lexer.range_from(factor_start);
                } else {
                    break;
                }
            }
        */
        Ok(Some(TermWithPos {
            term: factor,
            pos: factor_pos,
        }))
    }
}

fn prefix_operator(token: &Token) -> Option<&'static str> {
    match token {
        Token::Plus => Some("plus"),
        Token::Hyphen => Some("minus"),
        Token::Slash => Some("reciprocal"),
        Token::Exclamation => Some("logical_not"),
        Token::Tilde => Some("bitwise_not"),
        _ => None,
    }
}

#[derive(Clone, Copy, Sequence)]
enum Precedence {
    LogicalOr,
    LogicalAnd,
    Equality,
    Inequality,
    BitOr,
    BitXor,
    BitAnd,
    BitShift,
    AddSub,
    MulDivRem,
    TimeShift,
}

fn infix_operator(token: &Token, precedence: Precedence) -> Option<&'static str> {
    match (token, precedence) {
        (Token::Asterisk, Precedence::MulDivRem) => Some("mul"),
        (Token::Slash, Precedence::MulDivRem) => Some("div"),
        (Token::Percent, Precedence::MulDivRem) => Some("rem"),
        (Token::Plus, Precedence::AddSub) => Some("add"),
        (Token::Hyphen, Precedence::AddSub) => Some("sub"),
        (Token::DoubleGreater, Precedence::BitShift) => Some("right_shift"),
        (Token::DoubleLess, Precedence::BitShift) => Some("left_shift"),
        (Token::Ampersand, Precedence::BitAnd) => Some("bitwise_and"),
        (Token::Circumflex, Precedence::BitXor) => Some("bitwise_xor"),
        (Token::Bar, Precedence::BitOr) => Some("bitwise_or"),
        (Token::Greater, Precedence::Inequality) => Some("greater"),
        (Token::GreaterEqual, Precedence::Inequality) => Some("greater_or_equal"),
        (Token::Less, Precedence::Inequality) => Some("less"),
        (Token::LessEqual, Precedence::Inequality) => Some("less_or_equal"),
        (Token::DoubleEqual, Precedence::Equality) => Some("equal"),
        (Token::ExclamationEqual, Precedence::Equality) => Some("not_equal"),
        _ => None,
    }
}

fn assignment_operator(token: &Token) -> Option<&'static str> {
    match token {
        Token::Equal => Some("assign"),
        Token::PlusEqual => Some("add_assign"),
        Token::HyphenEqual => Some("sub_assign"),
        Token::AsteriskEqual => Some("mul_assign"),
        Token::SlashEqual => Some("div_assign"),
        Token::PercentEqual => Some("rem_assign"),
        Token::DoubleGreaterEqual => Some("right_shift_assign"),
        Token::DoubleLessEqual => Some("left_shift_assign"),
        Token::AmpersandEqual => Some("bitwise_and_assign"),
        Token::CircumflexEqual => Some("bitwise_xor_assign"),
        Token::BarEqual => Some("bitwise_or_assign"),
        _ => None,
    }
}

#[derive(Debug)]
enum Item {
    Import(usize),
    Function(Vec<usize>),
    Type(usize),
    GlobalVariable(usize),
}

enum Type {
    Builtin,
    Struct(usize),
}

struct StructDefinition {}

struct FunctionDefinition {
    path: PathBuf,
    opt_type_parameters: Option<Vec<ListElement>>,
    opt_parameters: Option<Vec<ListElement>>,
    opt_ret_ty: Option<RetTy>,
    body: Vec<Stmt>,
}

struct RetTy {
    arrow_pos: Pos,
    opt_ret_ty: Option<TermWithPos>,
}

enum Stmt {
    Var(TermWithPos),
    Term(TermWithPos),
    While(TermWithPos, Vec<Stmt>),
}

#[derive(PartialEq, Eq, Debug)]
struct TermWithPos {
    term: Term,
    pos: Pos,
}

#[derive(PartialEq, Eq, Debug)]
enum Term {
    NumericLiteral(String),
    StringLiteral(Vec<StringLiteralComponent>),
    IntegerTy,
    FloatTy,
    Identity,
    Identifier(String),
    MethodName(String),
    FieldByName {
        term_left: Box<TermWithPos>,
        name: String,
    },
    FieldByNumber {
        term_left: Box<TermWithPos>,
        number: String,
    },
    TypeAnnotation {
        term_left: Box<TermWithPos>,
        colon_pos: Pos,
        opt_term_right: Option<Box<TermWithPos>>,
    },
    UnaryOperation {
        operator: Box<TermWithPos>,
        opt_operand: Option<Box<TermWithPos>>,
    },
    BinaryOperation {
        opt_left_operand: Option<Box<TermWithPos>>,
        operator: Box<TermWithPos>,
        opt_right_operand: Option<Box<TermWithPos>>,
    },
    Assignment {
        opt_left_hand_side: Option<Box<TermWithPos>>,
        operator: Box<TermWithPos>,
        opt_right_hand_side: Option<Box<TermWithPos>>,
    },
    Conjunction {
        opt_conditions: Vec<Option<TermWithPos>>,
        operators_pos: Vec<Pos>,
    },
    Disjunction {
        opt_conditions: Vec<Option<TermWithPos>>,
        operators_pos: Vec<Pos>,
    },
    Parenthesized {
        inner: Box<TermWithPos>,
    },
    Tuple {
        elements: Vec<ListElement>,
    },
    FunctionCall {
        function: Box<TermWithPos>,
        arguments: Vec<ListElement>,
    },
    TypeParameters {
        term_left: Box<TermWithPos>,
        parameters: Vec<ListElement>,
    },
    ReturnType {
        arrow_pos: Pos,
        args: Box<TermWithPos>,
        opt_ret: Option<Box<TermWithPos>>,
    },
}

#[derive(PartialEq, Eq, Debug)]
enum StringLiteralComponent {
    String(String),
    Term(Option<TermWithPos>),
}

#[derive(PartialEq, Eq, Debug)]
enum ListElement {
    NonEmpty(TermWithPos),
    Empty { comma_pos: Pos },
}

/**
 * Tokens.
 */
#[derive(Debug, PartialEq, Eq)]
pub enum Token {
    /**
     * /\[`0`-`9`\](\[`Ee`\]\[`+-`\]|\p{XIDC})*\/u.
     * Consists numeric literals.
     */
    Digits(String),
    /**
     * /`"`(\[^`{}"\\`\]|`\\`\[`0nrt"'\\`\]|`{{`|`}}`|`\${`[Term](super::Term)`}`)*`"`/.
     *
     * Since it can contain a [Term](super::Term), we need to call a parser
     * function in [`parser`].
     */
    StringLiteral(Vec<StringLiteralComponent>),
    KeywordImport,
    KeywordExport,
    KeywordStruct,
    KeywordFunc,
    KeywordMethod,
    KeywordIf,
    KeywordElse,
    KeywordWhile,
    KeywordBreak,
    KeywordContinue,
    KeywordReturn,
    KeywordEnd,
    KeywordVar,
    KeywordInt,
    KeywordFloat,
    Underscore,
    Identifier(String),
    Plus,
    PlusEqual,
    Hyphen,
    HyphenEqual,
    HyphenGreater,
    Asterisk,
    AsteriskEqual,
    Slash,
    SlashEqual,
    Percent,
    PercentEqual,
    Equal,
    DoubleEqual,
    EqualGreater,
    Exclamation,
    ExclamationEqual,
    Greater,
    GreaterEqual,
    DoubleGreater,
    DoubleGreaterEqual,
    Less,
    LessEqual,
    DoubleLess,
    DoubleLessEqual,
    Ampersand,
    AmpersandEqual,
    DoubleAmpersand,
    Bar,
    BarEqual,
    DoubleBar,
    Circumflex,
    CircumflexEqual,
    Dot,
    Colon,
    Semicolon,
    Comma,
    Question,
    Tilde,
    Dollar,
    OpeningParenthesis,
    ClosingParenthesis,
    OpeningBracket,
    ClosingBracket,
    OpeningBrace,
    ClosingBrace,
}

/**
 * A parser.
 */
pub struct Parser<'str, 'iter> {
    iter: &'iter mut CharsPeekable<'str>,
    next_token_info: Option<TokenInfo>,
    next_token_start: Index,
    prev_token_end: Index,
}

struct TokenInfo {
    token: Token,
    is_adjacent: bool,
    is_on_new_line: bool,
}

impl<'str, 'iter> Parser<'str, 'iter> {
    pub fn new(iter: &'iter mut CharsPeekable<'str>) -> Result<Self, ParseError> {
        let (first_token_start, first_token_info) = read_token(iter, true, true)?;
        Ok(Self {
            iter,
            next_token_info: first_token_info,
            next_token_start: first_token_start,
            prev_token_end: Index { line: 0, column: 0 },
        })
    }
}
impl Parser<'_, '_> {
    pub fn next_token_ref(&self) -> Option<&Token> {
        Some(&self.next_token_info.as_ref()?.token)
    }
    pub fn next_token_mut(&mut self) -> Option<&mut Token> {
        Some(&mut self.next_token_info.as_mut()?.token)
    }
    pub fn adjacent_token_ref(&self) -> Option<&Token> {
        let TokenInfo {
            token, is_adjacent, ..
        } = self.next_token_info.as_ref()?;
        is_adjacent.then_some(token)
    }
    pub fn adjacent_token_mut(&mut self) -> Option<&mut Token> {
        let TokenInfo {
            token, is_adjacent, ..
        } = self.next_token_info.as_mut()?;
        is_adjacent.then_some(token)
    }
    pub fn next_token_start(&self) -> Index {
        self.next_token_start
    }
    pub fn next_token_pos(&self) -> Pos {
        Pos {
            start: self.next_token_start,
            end: self.iter.peek_index(),
        }
    }
    pub fn range_from(&self, start: Index) -> Pos {
        Pos {
            start,
            end: self.prev_token_end,
        }
    }
    pub fn next_token_on_current_line_ref(&self) -> Option<&Token> {
        let TokenInfo {
            token,
            is_on_new_line,
            ..
        } = self.next_token_info.as_ref()?;
        (!is_on_new_line).then_some(token)
    }
    pub fn next_token_on_current_line_mut(&mut self) -> Option<&mut Token> {
        let TokenInfo {
            token,
            is_on_new_line,
            ..
        } = self.next_token_info.as_mut()?;
        (!*is_on_new_line).then_some(token)
    }
    pub fn has_remaining_token(&self) -> bool {
        self.next_token_info.is_some()
    }
    pub fn has_remaining_token_on_current_line(&self) -> bool {
        self.next_token_info
            .as_ref()
            .is_some_and(|token_info| !token_info.is_on_new_line)
    }
    pub fn consume_token(&mut self) -> Result<(), ParseError> {
        self.prev_token_end = self.iter.peek_index();
        let (token_start, token_info) = read_token(&mut self.iter, true, false)?;
        self.next_token_start = token_start;
        self.next_token_info = token_info;
        Ok(())
    }
}

fn read_token(
    iter: &mut CharsPeekable,
    mut is_adjacent: bool,
    mut is_on_new_line: bool,
) -> Result<(Index, Option<TokenInfo>), ParseError> {
    let (start_index, first_ch) = loop {
        let Some(ch) = iter.peek_char() else {
            return Ok((iter.peek_index(), None));
        };
        if ch.is_ascii_whitespace() {
            is_adjacent = false;
            if ch == '\n' {
                is_on_new_line = true
            }
            iter.consume();
        } else {
            break (iter.peek_index(), ch);
        }
    };
    iter.consume();
    let token = match first_ch {
        '0'..='9' => {
            let mut value = first_ch.to_string();
            let mut after_e = false;
            while let Some(ch) = iter.peek_char() {
                after_e = match ch {
                    'e' | 'E' => true,
                    '0'..='9' | 'a'..='z' | 'A'..='Z' | '_' => false,
                    '+' | '-' if after_e => false,
                    _ => break,
                };
                if ch != '_' {
                    value.push(ch);
                }
                iter.consume();
            }
            Token::Digits(value)
        }
        '"' => {
            let mut components = Vec::new();
            let mut buf = String::new();
            #[derive(PartialEq, Eq)]
            enum Action {
                Border,
                Char,
                Expr,
            }
            let mut prev_action = Action::Border;
            loop {
                let Some(mut ch) = iter.peek_char() else {
                    return Err(ParseError::UnterminatedStringLiteral { start_index });
                };
                let index = iter.peek_index();
                iter.consume();
                let action = match ch {
                    '"' => Action::Border,
                    '{' => {
                        if iter.consume_if('{') {
                            Action::Char
                        } else {
                            Action::Expr
                        }
                    }
                    '}' => {
                        if iter.consume_if('}') {
                            Action::Char
                        } else {
                            return Err(ParseError::UnmatchedClosingBraceInStringLiteral {
                                closing_brace_index: index,
                                start_index,
                            });
                        }
                    }
                    '\\' => {
                        let Some(next_ch) = iter.peek_char() else {
                            return Err(ParseError::UnterminatedStringLiteral { start_index });
                        };
                        iter.consume();
                        ch = match next_ch {
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            '"' => '\"',
                            '\\' => '\\',
                            '0' => '\0',
                            '\'' => '\'',
                            _ => {
                                return Err(ParseError::InvalidEscapeSequence {
                                    backslash_index: index,
                                });
                            }
                        };
                        Action::Char
                    }
                    _ => Action::Char,
                };
                if action == Action::Char {
                    buf.push(ch);
                } else if prev_action == Action::Char {
                    components.push(StringLiteralComponent::String(std::mem::take(&mut buf)))
                }
                if action == Action::Expr {
                    let (first_token_start, first_token_info) = read_token(iter, true, false)?;
                    let mut parser = Parser {
                        iter,
                        next_token_info: first_token_info,
                        next_token_start: first_token_start,
                        prev_token_end: Index { line: 0, column: 0 },
                    };
                    let expr = parser.parse_disjunction(true)?;
                    components.push(StringLiteralComponent::Term(expr));
                } else if action == Action::Border {
                    break;
                }
                prev_action = action;
            }
            Token::StringLiteral(components)
        }
        _ if first_ch == '_' || unicode_ident::is_xid_start(first_ch) => {
            let mut name = first_ch.to_string();
            while let Some(ch) = iter.peek_char() {
                if unicode_ident::is_xid_continue(ch) {
                    name.push(ch);
                    iter.consume();
                } else {
                    break;
                }
            }
            match name.as_str() {
                "import" => Token::KeywordImport,
                "export" => Token::KeywordExport,
                "struct" => Token::KeywordStruct,
                "func" => Token::KeywordFunc,
                "method" => Token::KeywordMethod,
                "if" => Token::KeywordIf,
                "else" => Token::KeywordElse,
                "while" => Token::KeywordWhile,
                "break" => Token::KeywordBreak,
                "continue" => Token::KeywordContinue,
                "return" => Token::KeywordReturn,
                "end" => Token::KeywordEnd,
                "var" => Token::KeywordVar,
                "int" => Token::KeywordInt,
                "float" => Token::KeywordFloat,
                "_" => Token::Underscore,
                _ => Token::Identifier(name),
            }
        }
        '+' => {
            if iter.consume_if('=') {
                Token::PlusEqual
            } else {
                Token::Plus
            }
        }
        '-' => {
            if iter.consume_if('-') {
                skip_line_comment(iter);
                return read_token(iter, false, true);
            } else if iter.consume_if('=') {
                Token::HyphenEqual
            } else if iter.consume_if('>') {
                Token::HyphenGreater
            } else {
                Token::Hyphen
            }
        }
        '*' => {
            if iter.consume_if('=') {
                Token::AsteriskEqual
            } else {
                Token::Asterisk
            }
        }
        '/' => {
            if iter.consume_if('-') {
                skip_block_comment(iter, start_index, '/', '-', '-', '/')?;
                return read_token(iter, false, is_on_new_line);
            } else if iter.consume_if('/') {
                if !is_on_new_line {
                    return Err(ParseError::InvalidBlockComment { start_index });
                }
                skip_block_comment(iter, start_index, '/', '/', '\\', '\\')?;
                skip_line_comment(iter);
                return read_token(iter, false, true);
            } else if iter.consume_if('=') {
                Token::SlashEqual
            } else {
                Token::Slash
            }
        }
        '%' => {
            if iter.consume_if('=') {
                Token::PercentEqual
            } else {
                Token::Percent
            }
        }
        '=' => {
            if iter.consume_if('=') {
                Token::DoubleEqual
            } else if iter.consume_if('>') {
                Token::EqualGreater
            } else {
                Token::Equal
            }
        }
        '!' => {
            if iter.consume_if('=') {
                Token::ExclamationEqual
            } else {
                Token::Exclamation
            }
        }
        '>' => {
            if iter.consume_if('>') {
                if iter.consume_if('=') {
                    Token::DoubleGreaterEqual
                } else {
                    Token::DoubleGreater
                }
            } else if iter.consume_if('=') {
                Token::GreaterEqual
            } else {
                Token::Greater
            }
        }
        '<' => {
            if iter.consume_if('<') {
                if iter.consume_if('=') {
                    Token::DoubleLessEqual
                } else {
                    Token::DoubleLess
                }
            } else if iter.consume_if('=') {
                Token::LessEqual
            } else {
                Token::Less
            }
        }
        '&' => {
            if iter.consume_if('&') {
                Token::DoubleAmpersand
            } else if iter.consume_if('=') {
                Token::AmpersandEqual
            } else {
                Token::Ampersand
            }
        }
        '|' => {
            if iter.consume_if('|') {
                Token::DoubleBar
            } else if iter.consume_if('=') {
                Token::BarEqual
            } else {
                Token::Bar
            }
        }
        '^' => {
            if iter.consume_if('=') {
                Token::CircumflexEqual
            } else {
                Token::Circumflex
            }
        }
        ':' => Token::Colon,
        ';' => Token::Semicolon,
        ',' => Token::Comma,
        '?' => Token::Question,
        '~' => Token::Tilde,
        '(' => Token::OpeningParenthesis,
        ')' => Token::ClosingParenthesis,
        '[' => Token::OpeningBracket,
        ']' => Token::ClosingBracket,
        '{' => Token::OpeningBrace,
        '}' => Token::ClosingBrace,
        '.' => Token::Dot,
        '$' => Token::Dollar,
        _ => return Err(ParseError::UnexpectedCharacter(start_index)),
    };
    Ok((
        start_index,
        Some(TokenInfo {
            token,
            is_on_new_line,
            is_adjacent,
        }),
    ))
}

fn skip_line_comment(iter: &mut CharsPeekable) {
    loop {
        let ch = iter.peek_char();
        iter.consume();
        if let None | Some('\n') = ch {
            break;
        }
    }
}

fn skip_block_comment(
    iter: &mut CharsPeekable,
    start_index: Index,
    start0: char,
    start1: char,
    end0: char,
    end1: char,
) -> Result<(), ParseError> {
    let mut starts_index = vec![start_index];
    loop {
        let Some(ch) = iter.peek_char() else {
            return Err(ParseError::UnterminatedComment { starts_index });
        };
        let index = iter.peek_index();
        iter.consume();
        if ch == start0 && iter.consume_if(start1) {
            starts_index.push(index);
        } else if ch == end0 && iter.consume_if(end1) {
            starts_index.pop();
            if starts_index.is_empty() {
                return Ok(());
            }
        }
    }
}
