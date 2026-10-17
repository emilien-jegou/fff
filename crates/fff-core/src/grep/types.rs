use crate::types::{FileItem, MatchType};
use smallvec::SmallVec;

pub use crate::constants::MAX_FFFILE_SIZE;

/// Controls case sensitivity of the grep pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Casing {
    /// Case-insensitive unless the pattern contains an uppercase char.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

impl Casing {
    pub fn is_insensitive_for(self, pattern: &str) -> bool {
        match self {
            Casing::Smart => !pattern.chars().any(|c| c.is_uppercase()),
            Casing::Insensitive => true,
            Casing::Sensitive => false,
        }
    }
}

/// Controls how the grep pattern is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrepMode {
    /// Literal plain text match: default path that doesn't require any regex machinery
    #[default]
    PlainText,
    /// Smart fuzzy mode, allows user to make either a couple of single char typos or long gaps
    /// e.g. shcema -> shcema, or UserController -> UserAuthController
    ///
    /// Significatnly slower than plain text, especially on unindexed FilePicker
    Fuzzy,
}

/// A single content match within a file
#[derive(Debug, Clone)]
pub struct GrepMatch {
    /// Index into the deduplicated `files` vec of the GrepResult.
    pub file_index: usize,
    /// 1-based line number.
    pub line_number: u64,
    /// 0-based byte column of first match start within the line.
    pub col: usize,
    /// Absolute byte offset of the matched line from the start of the file.
    /// Can be used by the preview to seek directly without scanning from the top.
    pub byte_offset: u64,
    /// The matched line text, truncated to `MAX_LINE_DISPLAY_LEN`.
    pub line_content: String,
    /// Byte offsets `(start, end)` within `line_content` for each match.
    /// Stack-allocated for the common case of ≤4 spans per line.
    pub match_byte_offsets: SmallVec<[(u32, u32); 4]>,
    /// Fuzzy match score from neo_frizbee (only set in Fuzzy grep mode).
    pub fuzzy_score: Option<u16>,
    /// Classification of this match: Case-sensitive, Full, or Partial (fuzzy).
    pub match_type: MatchType,
    /// Lines before the match (for context display). Empty when context is 0.
    pub context_before: Vec<String>,
    /// Lines after the match (for context display). Empty when context is 0.
    pub context_after: Vec<String>,
}

impl GrepMatch {
    /// Strip leading whitespace from `line_content` and all context lines,
    /// adjusting `col` and `match_byte_offsets` so highlights remain correct.
    pub fn trim_leading_whitespace(&mut self) {
        let strip_len = self.line_content.len() - self.line_content.trim_start().len();
        if strip_len > 0 {
            self.line_content.drain(..strip_len);
            let off = strip_len as u32;
            self.col = self.col.saturating_sub(strip_len);
            for range in &mut self.match_byte_offsets {
                range.0 = range.0.saturating_sub(off);
                range.1 = range.1.saturating_sub(off);
            }
        }
        for line in &mut self.context_before {
            let n = line.len() - line.trim_start().len();
            if n > 0 {
                line.drain(..n);
            }
        }
        for line in &mut self.context_after {
            let n = line.len() - line.trim_start().len();
            if n > 0 {
                line.drain(..n);
            }
        }
    }
}

/// Options for grep search.
#[derive(Debug, Clone)]
pub struct GrepSearchOptions {
    pub max_file_size: u64,
    pub max_matches_per_file: usize,
    /// Legacy toggle: `true` => `Casing::Smart`, `false` => `Casing::Sensitive`.
    /// Ignored when `casing` is `Some`.
    pub smart_case: bool,
    /// Explicit case mode; overrides `smart_case` when set.
    pub casing: Option<Casing>,
    /// File-based pagination offset: index into the sorted/filtered file list
    /// to start searching from. Pass 0 for the first page, then use
    /// `GrepResult::next_file_offset` for subsequent pages.
    pub file_offset: usize,
    /// Maximum number of matches to collect before stopping.
    pub page_limit: usize,
    /// How to interpret the search pattern. Defaults to `PlainText`.
    pub mode: GrepMode,
    /// Number of context lines to include before each match. 0 = disabled.
    pub before_context: usize,
    /// Number of context lines to include after each match. 0 = disabled.
    pub after_context: usize,
    /// Strip leading whitespace from matched lines and context lines, adjusting
    /// highlight byte offsets accordingly. Useful for AI/MCP consumers and UIs
    /// that don't need indentation. Default: false.
    pub trim_whitespace: bool,
}

impl GrepSearchOptions {
    pub fn effective_casing(&self) -> Casing {
        match self.casing {
            Some(mode) => mode,
            None if self.smart_case => Casing::Smart,
            None => Casing::Sensitive,
        }
    }
}

impl Default for GrepSearchOptions {
    fn default() -> Self {
        Self {
            max_file_size: MAX_FFFILE_SIZE,
            max_matches_per_file: 200,
            smart_case: true,
            casing: None,
            file_offset: 0,
            page_limit: 50,
            mode: GrepMode::default(),
            before_context: 0,
            after_context: 0,
            trim_whitespace: false,
        }
    }
}

/// Result of a grep search with a list of matches, list of matched files, and metadata.
#[derive(Debug, Clone, Default)]
pub struct GrepResult<'a> {
    pub matches: Vec<GrepMatch>,
    /// Deduplicated file references for the returned matches.
    pub files: Vec<&'a FileItem>,
    /// Number of files actually searched in this call.
    pub total_files_searched: usize,
    /// Total number of indexed files (before filtering).
    pub total_files: usize,
    /// Total number of searchable files (after filtering out binary, too-large, etc.).
    pub filtered_file_count: usize,
    /// Number of files that contained at least one match.
    pub files_with_matches: usize,
    /// The file offset to pass for the next page. `0` if there are no more files.
    /// Callers should store this and pass it as `file_offset` in the next call.
    pub next_file_offset: usize,
}

impl<'a> GrepResult<'a> {
    /// Empty result carrying only the file counts (empty query / prefilter miss)
    pub(crate) fn empty(total_files: usize, filtered_file_count: usize) -> Self {
        Self {
            total_files,
            filtered_file_count,
            ..Default::default()
        }
    }

    pub(crate) fn collect(
        per_file_results: Vec<(usize, &'a FileItem, Vec<GrepMatch>)>,
        files_to_search_len: usize,
        options: &GrepSearchOptions,
        total_files: usize,
        filtered_file_count: usize,
        abort_resume: Option<usize>,
    ) -> Self {
        let page_limit = options.page_limit;

        let mut result_files: Vec<&'a FileItem> = Vec::new();
        let mut all_matches: Vec<GrepMatch> = Vec::new();
        let mut files_consumed: usize = 0;
        let mut page_filled = false;

        for (batch_idx, file, file_matches) in per_file_results {
            if abort_resume.is_some_and(|at| batch_idx >= at) {
                continue;
            }

            files_consumed = batch_idx + 1;

            let file_result_idx = result_files.len();
            result_files.push(file);

            for mut m in file_matches {
                m.file_index = file_result_idx;
                if options.trim_whitespace {
                    m.trim_leading_whitespace();
                }
                all_matches.push(m);
            }

            if all_matches.len() >= page_limit {
                page_filled = true;
                break;
            }
        }

        match abort_resume {
            Some(resume_at) if !page_filled => files_consumed = resume_at.min(files_to_search_len),
            None if result_files.is_empty() => files_consumed = files_to_search_len,
            _ => {}
        }

        let has_more = files_consumed < files_to_search_len
            && (abort_resume.is_some() || all_matches.len() >= page_limit);

        let next_file_offset = if has_more {
            options.file_offset + files_consumed
        } else {
            0
        };

        Self {
            matches: all_matches,
            files_with_matches: result_files.len(),
            files: result_files,
            total_files_searched: files_consumed,
            total_files,
            filtered_file_count,
            next_file_offset,
        }
    }
}
