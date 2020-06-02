use super::elements::*;
use super::tokens::*;
use crate::parsing::charstate::CharStateMachine;
use crate::parsing::inline::ParseInline;
use crate::parsing::placeholders::ProcessPlaceholders;
use crossbeam_utils::sync::WaitGroup;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fmt::{Display, Formatter};
use std::fs::read_to_string;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use termion::color::{self, Fg};
use termion::style;

macro_rules! parse_option {
    ($option:expr, $index:expr) => {
        if let Some(_) = $option {
        } else {
            return Err(ParseError::new($index));
        }
    };
}

#[derive(Debug)]
pub struct ParseError {
    index: usize,
    message: Option<String>,
}
impl Display for ParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(message) = &self.message {
            write!(
                f,
                "{} Parse Error at index {}: {}{}",
                Fg(color::Red),
                self.index,
                message,
                style::Reset
            )
        } else {
            write!(
                f,
                "{} Parse Error at index {}{}",
                Fg(color::Red),
                self.index,
                style::Reset
            )
        }
    }
}
impl Error for ParseError {}
impl ParseError {
    pub fn new(index: usize) -> Self {
        Self {
            index,
            message: None,
        }
    }

    pub fn new_with_message(index: usize, message: &str) -> Self {
        Self {
            index,
            message: Some(message.to_string()),
        }
    }

    pub fn set_message(&mut self, message: &str) {
        self.message = Some(message.to_string());
    }

    pub fn get_position(&self, content: &str) -> Option<(usize, usize)> {
        if content.len() <= self.index {
            return None;
        }
        let split_content = content.split_at(self.index);
        let line_number = split_content.0.matches("\n").count() as usize;
        let overshoot_position = self.index as isize - split_content.0.len() as isize;

        if let Some(line) = split_content.0.lines().last() {
            let inline_position = (line.len() as isize + overshoot_position) as usize;

            Some((line_number, inline_position))
        } else {
            None
        }
    }
}

pub struct Parser {
    pub(crate) index: usize,
    pub(crate) text: Vec<char>,
    pub(crate) current_char: char,
    section_nesting: u8,
    sections: Vec<u8>,
    section_return: Option<u8>,
    path: Option<String>,
    paths: Arc<Mutex<Vec<String>>>,
    wg: WaitGroup,
    is_child: bool,
    pub(crate) inline_break_at: Vec<char>,
    document: Document,
}

impl Parser {
    pub fn new_from_file(path: String) -> Result<Self, io::Error> {
        let content = read_to_string(path.clone())?;
        Ok(Self::new(content, Some(path)))
    }

    pub fn new(text: String, path: Option<String>) -> Self {
        Parser::create(text, path, Arc::new(Mutex::new(Vec::new())), false)
    }

    pub fn new_as_child(text: String, path: String, paths: Arc<Mutex<Vec<String>>>) -> Self {
        Self::create(text, Some(path), paths, true)
    }

    fn create(
        text: String,
        path: Option<String>,
        paths: Arc<Mutex<Vec<String>>>,
        is_child: bool,
    ) -> Self {
        let mut text: Vec<char> = text.chars().collect();
        text.append(&mut vec!['\n', ' ', '\n']); // push space and newline of eof. it fixes stuff and I don't know why.
        let current_char = text.get(0).unwrap().clone();
        if let Some(path) = path.clone() {
            let path_info = Path::new(&path);
            paths
                .lock()
                .unwrap()
                .push(path_info.to_str().unwrap().to_string())
        }
        Self {
            index: 0,
            text,
            current_char,
            sections: Vec::new(),
            section_nesting: 0,
            section_return: None,
            path,
            paths,
            wg: WaitGroup::new(),
            is_child,
            inline_break_at: Vec::new(),
            document: Document::new(!is_child),
        }
    }

    fn get_text(&self) -> String {
        self.text
            .iter()
            .fold("".to_string(), |a, b| format!("{}{}", a, b))
    }

    /// transform an import path to be relative to the current parsers file
    fn transform_path(&mut self, path: String) -> String {
        let mut path = path;
        let first_path_info = Path::new(&path);
        if first_path_info.is_absolute() {
            return first_path_info.to_str().unwrap().to_string();
        }
        if let Some(selfpath) = &self.path {
            let path_info = Path::new(&selfpath);
            if path_info.is_file() {
                if let Some(dir) = path_info.parent() {
                    path = format!("{}/{}", dir.to_str().unwrap(), path);
                }
            }
        }
        let path_info = Path::new(&path);
        return path_info.to_str().unwrap().to_string();
    }

    /// starts up a new thread to parse the imported document
    fn import_document(&mut self, path: String) -> Result<Arc<Mutex<ImportAnchor>>, ParseError> {
        let path = self.transform_path(path);
        let path_info = Path::new(&path);
        if !path_info.exists() || !path_info.is_file() {
            println!(
                "{}Import of \"{}\" failed: The file doesn't exist.{}",
                Fg(color::Yellow),
                path,
                style::Reset
            );
            return Err(ParseError::new_with_message(
                self.index,
                "file does not exist",
            ));
        }
        {
            let mut paths = self.paths.lock().unwrap();
            if paths.iter().find(|item| **item == path) != None {
                println!(
                    "{}Import of \"{}\" failed: Cyclic import.{}",
                    Fg(color::Yellow),
                    path,
                    style::Reset
                );
                return Err(ParseError::new_with_message(self.index, "cyclic import"));
            }
            paths.push(path.clone());
        }
        let anchor = Arc::new(Mutex::new(ImportAnchor::new()));
        let anchor_clone = Arc::clone(&anchor);
        let wg = self.wg.clone();
        let paths = Arc::clone(&self.paths);

        let _ = thread::spawn(move || {
            let text = read_to_string(path.clone()).unwrap();

            let mut parser = Parser::new_as_child(text.to_string(), path, paths);
            let document = parser.parse();
            anchor_clone.lock().unwrap().set_document(document);

            drop(wg);
        });

        Ok(anchor)
    }

    /// parses the given text into a document
    pub fn parse(&mut self) -> Document {
        self.document.path = self.path.clone();

        while self.index < self.text.len() {
            match self.parse_block() {
                Ok(block) => self.document.add_element(block),
                Err(err) => {
                    if let Some(path) = &self.path {
                        if let Some(position) = err.get_position(&self.get_text()) {
                            println!(
                                "{} Error in File {}:{}:{} - {}",
                                Fg(color::Red),
                                path,
                                position.0,
                                position.1,
                                err
                            );
                        } else {
                            println!("{} Error in File {}: {}", Fg(color::Red), path, err);
                        }
                    } else {
                        println!("{}", err);
                    }
                    break;
                }
            }
        }

        let wg = self.wg.clone();
        self.wg = WaitGroup::new();
        wg.wait();
        if !self.is_child {
            self.document.process_placeholders();
        }
        let document = self.document.clone();
        self.document = Document::new(!self.is_child);

        document
    }

    /// Parses a block Token
    pub fn parse_block(&mut self) -> Result<Block, ParseError> {
        if let Some(section) = self.section_return {
            if section <= self.section_nesting && (self.section_nesting > 0) {
                return Err(ParseError::new_with_message(
                    self.index,
                    "invalid section nesting",
                ));
            } else {
                self.section_return = None;
            }
        }
        let token = if let Ok(section) = self.parse_section() {
            Block::Section(section)
        } else if let Some(_) = self.section_return {
            return Err(ParseError::new(self.index));
        } else if let Ok(list) = self.parse_list() {
            Block::List(list)
        } else if let Ok(table) = self.parse_table() {
            Block::Table(table)
        } else if let Ok(code_block) = self.parse_code_block() {
            Block::CodeBlock(code_block)
        } else if let Ok(quote) = self.parse_quote() {
            Block::Quote(quote)
        } else if let Ok(import) = self.parse_import() {
            Block::Import(import)
        } else if let Some(_) = self.section_return {
            return Err(ParseError::new(self.index));
        } else if let Ok(pholder) = self.parse_placeholder() {
            Block::Placeholder(pholder)
        } else if let Ok(paragraph) = self.parse_paragraph() {
            Block::Paragraph(paragraph)
        } else {
            return Err(ParseError::new(self.index));
        };

        Ok(token)
    }

    /// Parses a section that consists of a header and one or more blocks
    fn parse_section(&mut self) -> Result<Section, ParseError> {
        let start_index = self.index;
        self.seek_whitespace();
        if self.check_special(&HASH) {
            let mut size = 1;
            while let Some(_) = self.next_char() {
                if !self.check_special(&HASH) {
                    break;
                }
                size += 1;
            }
            let mut metadata = None;
            if let Ok(meta) = self.parse_inline_metadata() {
                metadata = Some(meta);
            }
            if size <= self.section_nesting || !self.current_char.is_whitespace() {
                if size <= self.section_nesting {
                    self.section_return = Some(size);
                }
                return Err(self.revert_with_error(start_index));
            }
            self.seek_inline_whitespace();
            let mut header = self.parse_header()?;
            header.size = size;
            self.section_nesting = size;
            self.sections.push(size);
            let mut section = Section::new(header);
            section.metadata = metadata;
            self.seek_whitespace();

            while let Ok(block) = self.parse_block() {
                section.add_element(block);
            }

            self.sections.pop();
            if let Some(sec) = self.sections.last() {
                self.section_nesting = *sec
            } else {
                self.section_nesting = 0;
            }
            Ok(section)
        } else {
            return Err(self.revert_with_error(start_index));
        }
    }

    /// parses the header of a section
    fn parse_header(&mut self) -> Result<Header, ParseError> {
        let start_index = self.index;
        let line = self.parse_line()?;
        let mut anchor = String::new();
        self.text[start_index..self.index]
            .iter()
            .for_each(|e| anchor.push(*e));
        anchor.retain(|c| !c.is_whitespace());
        Ok(Header::new(line, anchor))
    }

    /// parses a code block
    fn parse_code_block(&mut self) -> Result<CodeBlock, ParseError> {
        self.seek_whitespace();
        self.assert_special_sequence(&SQ_CODE_BLOCK, self.index)?;
        self.skip_char();
        let language = self.get_string_until(&[LB], &[])?;
        self.skip_char();
        let text = self.get_string_until_sequence(&[&SQ_CODE_BLOCK], &[])?;
        for _ in 0..2 {
            self.skip_char();
        }

        Ok(CodeBlock {
            language,
            code: text,
        })
    }

    /// parses a quote
    fn parse_quote(&mut self) -> Result<Quote, ParseError> {
        let start_index = self.index;
        self.seek_whitespace();
        let metadata = if let Ok(meta) = self.parse_inline_metadata() {
            Some(meta)
        } else {
            None
        };
        if self.check_special(&META_CLOSE) {
            if self.next_char() == None {
                return Err(self.revert_with_error(start_index));
            }
        }
        let mut quote = Quote::new(metadata);

        while self.check_special(&QUOTE_START)
            && self.next_char() != None
            && (self.check_seek_inline_whitespace() || self.check_special(&LB))
        {
            if let Ok(text) = self.parse_text_line() {
                if text.subtext.len() > 0 {
                    quote.add_text(text);
                }
            } else {
                break;
            }
        }
        if quote.text.len() == 0 {
            return Err(self.revert_with_error(start_index));
        }

        Ok(quote)
    }

    /// Parses metadata
    pub(crate) fn parse_inline_metadata(&mut self) -> Result<InlineMetadata, ParseError> {
        let start_index = self.index;
        self.assert_special(&META_OPEN, start_index)?;
        self.skip_char();

        let mut values = HashMap::new();
        while let Ok((key, value)) = self.parse_metadata_pair() {
            values.insert(key, value);
            if self.check_special(&META_CLOSE) || self.check_linebreak() {
                // abort the parsing of the inner content when encountering a closing tag or linebreak
                break;
            }
        }
        if self.check_special(&META_CLOSE) {
            self.skip_char();
        }
        if values.len() == 0 {
            // if there was a linebreak (the metadata wasn't closed) or there is no inner data
            // return an error
            return Err(self.revert_with_error(start_index));
        }

        Ok(InlineMetadata { data: values })
    }

    /// parses a key-value metadata pair
    fn parse_metadata_pair(&mut self) -> Result<(String, MetadataValue), ParseError> {
        self.seek_inline_whitespace();
        let name = self.get_string_until(&[META_CLOSE, EQ, SPACE, LB], &[])?;

        self.seek_inline_whitespace();
        let mut value = MetadataValue::Bool(true);
        if self.check_special(&EQ) {
            self.skip_char();
            self.seek_inline_whitespace();
            if let Ok(ph) = self.parse_placeholder() {
                value = MetadataValue::Placeholder(ph);
            } else {
                let quoted_string = self.check_special_group(&QUOTES);
                let parse_until = if quoted_string {
                    let quote_start = self.current_char;
                    self.skip_char();
                    vec![quote_start, META_CLOSE, LB]
                } else {
                    vec![META_CLOSE, LB, SPACE]
                };
                let raw_value = self.get_string_until(&parse_until, &[])?;
                if self.check_special_group(&QUOTES) {
                    self.skip_char();
                }
                value = if quoted_string {
                    MetadataValue::String(raw_value)
                } else if raw_value.to_lowercase().as_str() == "true" {
                    MetadataValue::Bool(true)
                } else if raw_value.to_lowercase().as_str() == "false" {
                    MetadataValue::Bool(false)
                } else if let Ok(num) = raw_value.parse::<i64>() {
                    MetadataValue::Integer(num)
                } else if let Ok(num) = raw_value.parse::<f64>() {
                    MetadataValue::Float(num)
                } else {
                    MetadataValue::String(raw_value)
                }
            }
        }

        Ok((name, value))
    }

    /// parses an import and starts a new task to parse the document of the import
    fn parse_import(&mut self) -> Result<Import, ParseError> {
        let start_index = self.index;
        self.seek_whitespace();
        self.assert_special_sequence_group(&[&[IMPORT_START, IMPORT_OPEN]], start_index)?;
        let mut path = String::new();
        while let Some(character) = self.next_char() {
            if self.check_linebreak() || self.check_special(&IMPORT_CLOSE) {
                break;
            }
            path.push(character);
        }
        if self.check_linebreak() || path.is_empty() {
            return Err(self.revert_with_error(start_index));
        }
        if self.check_special(&IMPORT_CLOSE) {
            parse_option!(self.next_char(), self.index);
        }
        // parsing success

        if self.section_nesting > 0 {
            self.section_return = Some(0);
            let err = ParseError::new_with_message(self.index, "import section nesting error");
            self.revert_to(start_index)?;
            return Err(err);
        }

        self.seek_whitespace();

        if let Ok(anchor) = self.import_document(path.clone()) {
            Ok(Import { path, anchor })
        } else {
            Err(ParseError::new(self.index))
        }
    }

    /// Parses a paragraph
    fn parse_paragraph(&mut self) -> Result<Paragraph, ParseError> {
        self.seek_whitespace();
        let mut paragraph = Paragraph::new();
        while let Ok(token) = self.parse_line() {
            paragraph.add_element(token);
            let start_index = self.index;
            if self.check_special_sequence_group(&BLOCK_SPECIAL_CHARS) {
                self.revert_to(start_index)?;
                break;
            }
            self.revert_to(start_index)?;
        }

        if paragraph.elements.len() > 0 {
            Ok(paragraph)
        } else {
            Err(ParseError::new(self.index))
        }
    }

    /// parses a list which consists of one or more list items
    /// The parsing is done iterative to resolve nested items
    fn parse_list(&mut self) -> Result<List, ParseError> {
        let mut list = List::new();
        let start_index = self.index;
        self.seek_whitespace();

        let ordered = if self.check_special_group(&LIST_SPECIAL_CHARS) {
            false
        } else {
            true
        };
        list.ordered = ordered;
        let mut list_hierarchy: Vec<ListItem> = Vec::new();
        while let Ok(mut item) = self.parse_list_item() {
            while let Some(parent_item) = list_hierarchy.pop() {
                if parent_item.level < item.level {
                    // the parent item is the actual parent of the next item
                    list_hierarchy.push(parent_item);
                    break;
                } else if parent_item.level == item.level {
                    // the parent item is a sibling and has to be appended to a parent
                    if list_hierarchy.is_empty() {
                        list.add_item(parent_item);
                    } else {
                        let mut parent_parent = list_hierarchy.pop().unwrap();
                        parent_parent.add_child(parent_item);
                        list_hierarchy.push(parent_parent);
                    }
                    break;
                } else {
                    // the parent item is a child of a sibling of the current item
                    if list_hierarchy.is_empty() {
                        item.add_child(parent_item);
                    } else {
                        let mut parent_parent = list_hierarchy.pop().unwrap();
                        parent_parent.add_child(parent_item);
                        list_hierarchy.push(parent_parent);
                    }
                }
            }
            list_hierarchy.push(item);
        }

        // the remaining items in the hierarchy need to be combined
        while let Some(item) = list_hierarchy.pop() {
            if !list_hierarchy.is_empty() {
                let mut parent_item = list_hierarchy.pop().unwrap();
                parent_item.add_child(item);
                list_hierarchy.push(parent_item);
            } else {
                list_hierarchy.push(item);
                break;
            }
        }
        list.items.append(&mut list_hierarchy);

        if list.items.len() > 0 {
            Ok(list)
        } else {
            return Err(self.revert_with_error(start_index));
        }
    }

    /// parses a single list item defined with -
    fn parse_list_item(&mut self) -> Result<ListItem, ParseError> {
        let start_index = self.index;
        self.seek_inline_whitespace();
        let level = self.index - start_index;
        self.assert_special_group(&LIST_SPECIAL_CHARS, start_index)?;
        let ordered = self.current_char.is_numeric();
        self.skip_char();
        if self.check_special(&DOT) {
            self.skip_char();
        }
        if !self.check_seek_inline_whitespace() {
            return Err(self.revert_with_error(start_index));
        }
        self.seek_inline_whitespace();
        if self.check_special(&MINUS) {
            return Err(self.revert_with_error(start_index));
        }

        let item = ListItem::new(self.parse_line()?, level as u16, ordered);

        Ok(item)
    }

    /// parses a markdown table
    fn parse_table(&mut self) -> Result<Table, ParseError> {
        let header = self.parse_row()?;
        if self.check_linebreak() {
            self.skip_char();
        }
        let seek_index = self.index;
        let mut table = Table::new(header);
        while let Some(_) = self.next_char() {
            self.seek_inline_whitespace();
            if !self.check_special_group(&[MINUS, PIPE]) || self.check_linebreak() {
                break;
            }
        }

        if !self.check_linebreak() {
            self.revert_to(seek_index)?;
            return Ok(table);
        }

        self.seek_whitespace();
        while let Ok(row) = self.parse_row() {
            table.add_row(row);
        }

        Ok(table)
    }

    /// parses a table row/head
    pub fn parse_row(&mut self) -> Result<Row, ParseError> {
        let start_index = self.index;
        self.seek_inline_whitespace();
        self.assert_special(&PIPE, start_index)?;
        self.skip_char();
        if self.check_special(&PIPE) {
            return Err(self.revert_with_error(start_index));
        }
        self.inline_break_at.push(PIPE);

        self.seek_inline_whitespace();
        let mut row = Row::new();
        loop {
            let mut element = TextLine::new();
            while let Ok(inline) = self.parse_inline() {
                element.subtext.push(inline);
                if self.check_linebreak() || self.check_special(&PIPE) {
                    break;
                }
            }
            row.add_cell(Cell {
                text: Line::Text(element),
            });
            if self.check_special(&PIPE) {
                self.skip_char();
            }
            if self.check_linebreak() {
                break;
            }
            self.seek_inline_whitespace();
        }
        self.inline_break_at.clear();
        if self.check_special(&PIPE) {
            self.skip_char();
            self.skip_char();
        } else {
            self.skip_char();
        }

        if row.cells.len() > 0 {
            Ok(row)
        } else {
            return Err(self.revert_with_error(start_index));
        }
    }

    /// parses inline definitions
    fn parse_line(&mut self) -> Result<Line, ParseError> {
        if self.index > self.text.len() {
            Err(ParseError::new(self.index))
        } else {
            if let Ok(ruler) = self.parse_ruler() {
                Ok(Line::Ruler(ruler))
            } else if let Ok(centered) = self.parse_centered() {
                Ok(Line::Centered(centered))
            } else if let Ok(text) = self.parse_text_line() {
                Ok(Line::Text(text))
            } else {
                Err(ParseError::new(self.index))
            }
        }
    }

    /// parses centered text
    fn parse_centered(&mut self) -> Result<Centered, ParseError> {
        let start_index = self.index;
        self.assert_special_sequence(&SQ_CENTERED_START, start_index)?;
        self.skip_char();
        let line = self.parse_text_line()?;

        Ok(Centered { line })
    }

    /// parses a placeholder element
    pub(crate) fn parse_placeholder(&mut self) -> Result<Arc<Mutex<Placeholder>>, ParseError> {
        let start_index = self.index;
        self.assert_special_sequence(&SQ_PHOLDER_START, self.index)?;
        self.skip_char();
        let name = if let Ok(name_str) = self.get_string_until_sequence(&[&SQ_PHOLDER_STOP], &[LB])
        {
            name_str
        } else {
            return Err(self.revert_with_error(start_index));
        };
        self.skip_char();

        let metadata = if let Ok(meta) = self.parse_inline_metadata() {
            Some(meta)
        } else {
            None
        };

        let placeholder = Arc::new(Mutex::new(Placeholder::new(name, metadata)));
        self.document.add_placeholder(Arc::clone(&placeholder));

        Ok(placeholder)
    }

    /// parses a ruler
    fn parse_ruler(&mut self) -> Result<Ruler, ParseError> {
        let start_index = self.index;
        self.seek_inline_whitespace();
        self.assert_special_sequence(&SQ_RULER, start_index)?;
        self.seek_until_linebreak();
        Ok(Ruler {})
    }

    /// Parses a line of text
    fn parse_text_line(&mut self) -> Result<TextLine, ParseError> {
        let mut text = TextLine::new();
        while let Ok(subtext) = self.parse_inline() {
            text.add_subtext(subtext);
            let current_index = self.index;
            if self.next_char() == None {
                break;
            }
            self.revert_to(current_index)?;
        }

        if self.check_linebreak() {
            parse_option!(self.next_char(), self.index);
        }

        Ok(text)
    }
}