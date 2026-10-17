use crate::{
    index::constraints::apply_constraints,
    match_offsets::char_indices_to_byte_offsets,
    simd_path::{ArenaPtr, MAX_PATH_CHUNKS},
    sort_buffer::sort_with_buffer,
    types::{DirItem, FileItem, MatchType, Score, ScoringContext},
};
use fff_query_parser::{FFFQuery, FuzzyQuery};
use neo_frizbee::Scoring;
use rayon::prelude::*;
use smallvec::SmallVec;
use std::{borrow::Cow, path::MAIN_SEPARATOR};

enum FileItems<'a> {
    All(&'a [FileItem]),
    Filtered(Vec<&'a FileItem>),
}

impl<'a> FileItems<'a> {
    #[inline]
    fn len(&self) -> usize {
        match self {
            FileItems::All(s) => s.len(),
            FileItems::Filtered(v) => v.len(),
        }
    }

    #[inline]
    fn index(&self, index: usize) -> &'a FileItem {
        match self {
            FileItems::All(s) => &s[index],
            FileItems::Filtered(v) => v[index],
        }
    }
}

/// Resolve a FileItem's chunked path into frizbee's pointer buffer.
/// Returns `Some((chunk_count, byte_len))` or `None` for deleted files.
#[inline]
fn resolve_file_chunks(
    file: &FileItem,
    arena: ArenaPtr,
    buf: &mut [*const u8; MAX_PATH_CHUNKS],
) -> Option<(usize, u16)> {
    if file.is_deleted() {
        return None;
    }
    let ptrs = file.path.resolve_ptrs(arena, buf);
    Some((ptrs.len(), file.path.byte_len))
}

#[inline]
fn match_fuzzy_parts(
    fuzzy_parts: &[&str],
    working_files: &FileItems<'_>,
    options: &neo_frizbee::Config,
    max_threads: usize,
    arena: ArenaPtr,
) -> Vec<neo_frizbee::Match> {
    let valid_parts: SmallVec<[&str; 4]> = fuzzy_parts
        .iter()
        .copied()
        .filter(|p| p.len() >= 2)
        .collect();

    if valid_parts.is_empty() {
        tracing::debug!("match_fuzzy_parts: no valid parts after filtering, returning empty");
        return vec![];
    }

    let reverse_pair = valid_parts.len() == 2 && valid_parts[1].len() > valid_parts[0].len();
    let first_part = usize::from(reverse_pair);
    let mut first_options = *options;
    if reverse_pair {
        first_options.max_typos = options
            .max_typos
            .map(|t| t.min(valid_parts[1].len() as u16));
    }

    // Narrow two-part queries with the longer part before scoring the shorter part.
    let first_part_matches = match_file_range(
        valid_parts[first_part],
        working_files.len(),
        &|index, buf: &mut [*const u8; MAX_PATH_CHUNKS]| {
            resolve_file_chunks(working_files.index(index as usize), arena, buf)
        },
        &first_options,
        max_threads,
    );

    if valid_parts.len() == 1 {
        return first_part_matches;
    }

    let total_parts = valid_parts.len() as u32;
    let mut matches = first_part_matches;
    for (part_index, part) in valid_parts
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != first_part)
    {
        let mut part_options = *options;
        if part_index > 0 {
            part_options.max_typos = options.max_typos.map(|t| t.min(part.len() as u16));
        }

        // Match only the files that survived the previous round, addressed
        // through the previous matches without collecting a subset.
        let survivors = &matches;
        let sub_matches = match_file_range(
            part,
            survivors.len(),
            &|index, buf: &mut [*const u8; MAX_PATH_CHUNKS]| {
                let file = working_files.index(survivors[index as usize].index as usize);
                resolve_file_chunks(file, arena, buf)
            },
            &part_options,
            max_threads,
        );

        if sub_matches.is_empty() {
            return vec![]; // break early! 
        }

        // Map sub_matches back to original indices, average scores across all parts.
        matches = sub_matches
            .into_iter()
            .map(|sm| {
                let prev = &matches[sm.index as usize];
                let sum = (prev.score as u32).saturating_add(sm.score as u32);
                let avg = sum / total_parts;

                neo_frizbee::Match {
                    index: prev.index,
                    score: avg.min(u16::MAX as u32) as u16,
                    end_col: if reverse_pair {
                        sm.end_col
                    } else {
                        prev.end_col
                    },
                    exact: prev.exact && sm.exact,
                }
            })
            .collect();
    }

    matches
}

/// Match + score across base and overflow files, each with their own arena.
#[tracing::instrument(skip_all, level = tracing::Level::DEBUG)]
pub(crate) fn fuzzy_match_and_score_files<'a>(
    files: &'a [FileItem],
    context: &ScoringContext,
    base_count: usize,
    base_arena: ArenaPtr,
    overflow_arena: ArenaPtr,
) -> (Vec<&'a FileItem>, Vec<Score>, usize) {
    if fuzzy_parts(context.query).is_none() {
        return rank_by_frecency(files, context, base_count, base_arena, overflow_arena);
    }

    // Process overflow files first: newly added files (created after the
    // initial scan) live in the overflow arena and are more likely to be
    // relevant to the current search. Exact ties are broken by index order.
    let results = if files.len() > base_count {
        let mut results = match_and_score_in_arena(&files[base_count..], context, overflow_arena);

        results.extend(match_and_score_in_arena(
            &files[..base_count],
            context,
            base_arena,
        ));

        results
    } else {
        match_and_score_in_arena(files, context, base_arena)
    };

    sort_and_paginate(results, context, |score| score.total)
}

// Ranks on a 16-byte (file, total) pair and builds the full Score only for the page.
fn rank_by_frecency<'a>(
    files: &'a [FileItem],
    context: &ScoringContext,
    base_count: usize,
    base_arena: ArenaPtr,
    overflow_arena: ArenaPtr,
) -> (Vec<&'a FileItem>, Vec<Score>, usize) {
    let base_count = base_count.min(files.len());
    let mut totals: Vec<(&FileItem, i32)> = Vec::new();
    for (slice, arena) in [
        (&files[base_count..], overflow_arena),
        (&files[..base_count], base_arena),
    ] {
        if slice.is_empty() {
            continue;
        }
        let Some(working_files) = filter_by_constraints(slice, context, arena) else {
            continue;
        };
        frecency_totals(&working_files, context, arena, &mut totals);
    }

    let (items, _, total_matched) = sort_and_paginate(totals, context, |total| *total);
    let scores = items
        .iter()
        .map(|file| {
            let arena = if file.is_overflow() {
                overflow_arena
            } else {
                base_arena
            };
            frecency_score(file, context, arena)
        })
        .collect();
    (items, scores, total_matched)
}

fn fuzzy_parts<'q>(query: &'q FFFQuery<'q>) -> Option<&'q [&'q str]> {
    match &query.fuzzy_query {
        FuzzyQuery::Text(t) if t.len() >= 2 => Some(std::slice::from_ref(t)),
        FuzzyQuery::Parts(parts) if !parts.is_empty() => Some(parts.as_slice()),
        _ => None,
    }
}

// None when constraints exclude every file.
fn filter_by_constraints<'a>(
    files: &'a [FileItem],
    context: &ScoringContext,
    arena: ArenaPtr,
) -> Option<FileItems<'a>> {
    let constraints = &context.query.constraints;
    if constraints.is_empty() {
        return Some(FileItems::All(files));
    }
    match apply_constraints(files, constraints, arena, arena) {
        Some(filtered) if !filtered.is_empty() => Some(FileItems::Filtered(filtered)),
        Some(_) => None,
        None => Some(FileItems::All(files)),
    }
}

pub(crate) fn fuzzy_match_byte_offsets_for_page<'q>(
    query: &'q FFFQuery<'q>,
    items: &[&FileItem],
    max_typos: u16,
    base_arena: ArenaPtr,
    overflow_arena: ArenaPtr,
) -> Vec<SmallVec<[(u32, u32); 4]>> {
    let parts: SmallVec<[&str; 4]> = match &query.fuzzy_query {
        FuzzyQuery::Text(text) if text.len() >= 2 => smallvec::smallvec![*text],
        FuzzyQuery::Parts(parts) => parts.iter().copied().filter(|p| p.len() >= 2).collect(),
        _ => SmallVec::new(),
    };

    let mut ranges_by_item = vec![SmallVec::new(); items.len()];
    if parts.is_empty() || items.is_empty() {
        return ranges_by_item;
    }

    let mut paths = String::with_capacity(items.iter().map(|item| item.relative_path_len()).sum());
    for item in items {
        let arena = if item.is_overflow() {
            overflow_arena
        } else {
            base_arena
        };
        item.path.append_to_string(arena, &mut paths);
    }

    let mut start = 0;
    let path_strs: Vec<&str> = items
        .iter()
        .map(|item| {
            let end = start + item.relative_path_len();
            let path = &paths[start..end];
            start = end;
            path
        })
        .collect();

    let has_uppercase = parts
        .iter()
        .any(|part| part.chars().any(|ch| ch.is_uppercase()));
    let config = neo_frizbee::Config {
        max_typos: Some(max_typos),
        sort: neo_frizbee::SortStrategy::Unsorted,
        scoring: Scoring {
            capitalization_bonus: if has_uppercase { 8 } else { 0 },
            matching_case_bonus: if has_uppercase { 4 } else { 0 },
            ..Default::default()
        },
        ..Default::default()
    };

    for (idx, part) in parts.iter().copied().enumerate() {
        let mut part_config = config;
        if idx > 0 {
            part_config.max_typos = config.max_typos.map(|t| t.min(part.len() as u16));
        }

        let mut matcher = neo_frizbee::Matcher::new(part, &part_config);
        for mut matched in matcher.match_list_indices(&path_strs) {
            let item_idx = matched.index as usize;
            let Some(path) = path_strs.get(item_idx) else {
                continue;
            };

            matched.indices.sort_unstable();
            ranges_by_item[item_idx].extend(char_indices_to_byte_offsets(path, &matched.indices));
        }
    }

    for ranges in &mut ranges_by_item {
        *ranges = merge_byte_offsets(std::mem::take(ranges));
    }

    ranges_by_item
}

fn merge_byte_offsets(mut ranges: SmallVec<[(u32, u32); 4]>) -> SmallVec<[(u32, u32); 4]> {
    if ranges.len() <= 1 {
        return ranges;
    }

    ranges.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut merged = 0;
    for index in 0..ranges.len() {
        let (start, end) = ranges[index];
        if end <= start {
            continue;
        }

        if merged > 0 && start <= ranges[merged - 1].1 {
            let last = &mut ranges[merged - 1];
            last.1 = last.1.max(end);
            continue;
        }

        ranges[merged] = (start, end);
        merged += 1;
    }

    ranges.truncate(merged);
    ranges
}

/// Resolve a DirItem's chunked path into frizbee's pointer buffer.
#[inline]
fn resolve_dir_chunks(
    dir: &DirItem,
    arena: ArenaPtr,
    overflow_arena: ArenaPtr,
    buf: &mut [*const u8; MAX_PATH_CHUNKS],
) -> Option<(usize, u16)> {
    let arena = if dir.is_overflow() {
        overflow_arena
    } else {
        arena
    };
    let ptrs = dir.path.resolve_ptrs(arena, buf);
    Some((ptrs.len(), dir.path.byte_len))
}

/// Run SIMD fuzzy match across directory items, narrowing the candidate set
/// through each query part — same pipeline as file matching.
fn match_fuzzy_parts_dirs(
    fuzzy_parts: &[&str],
    working_dirs: &[&DirItem],
    options: &neo_frizbee::Config,
    max_threads: usize,
    arena: ArenaPtr,
    overflow_arena: ArenaPtr,
) -> Vec<neo_frizbee::Match> {
    let valid_parts: SmallVec<[&str; 4]> = fuzzy_parts
        .iter()
        .copied()
        .filter(|p| p.len() >= 2)
        .collect();

    if valid_parts.is_empty() {
        return vec![];
    }

    let first_part_matches = neo_frizbee::match_range_parallel_resolved(
        valid_parts[0],
        working_dirs.len(),
        &|index, buf: &mut [*const u8; MAX_PATH_CHUNKS]| {
            resolve_dir_chunks(working_dirs[index as usize], arena, overflow_arena, buf)
        },
        options,
        max_threads,
    );

    if valid_parts.len() == 1 {
        return first_part_matches;
    }

    let mut matches = first_part_matches;
    let total_parts = valid_parts.len() as u32;
    for part in valid_parts[1..].iter() {
        let mut part_options = *options;
        part_options.max_typos = options.max_typos.map(|t| t.min(part.len() as u16));

        // Match only the dirs that survived the previous round, addressed
        // through the previous matches without collecting a subset.
        let survivors = &matches;
        let sub_matches = neo_frizbee::match_range_parallel_resolved(
            part,
            survivors.len(),
            &|index, buf: &mut [*const u8; MAX_PATH_CHUNKS]| {
                let dir = working_dirs[survivors[index as usize].index as usize];
                resolve_dir_chunks(dir, arena, overflow_arena, buf)
            },
            &part_options,
            max_threads,
        );

        if sub_matches.is_empty() {
            return vec![];
        }

        // Map sub_matches back to original indices, average scores across all parts.
        matches = sub_matches
            .into_iter()
            .map(|sm| {
                let prev = &matches[sm.index as usize];
                let sum = (prev.score as u32).saturating_add(sm.score as u32);
                let avg = sum / total_parts;

                neo_frizbee::Match {
                    index: prev.index,
                    score: avg.min(u16::MAX as u32) as u16,
                    end_col: prev.end_col, // keep first part's position for dirname bonus
                    exact: prev.exact && sm.exact,
                }
            })
            .collect();
    }

    matches
}

/// Match + score directories against a fuzzy query.
/// Scoring: base_score, frecency_boost.
#[tracing::instrument(skip_all, level = tracing::Level::DEBUG)]
pub(crate) fn fuzzy_match_and_score_dirs<'a>(
    dirs: &'a [DirItem],
    context: &ScoringContext,
    arena: ArenaPtr,
    overflow_arena: ArenaPtr,
) -> (Vec<&'a DirItem>, Vec<Score>, usize) {
    if dirs.is_empty() {
        return (vec![], vec![], 0);
    }

    let parsed_query = context.query;
    // Ghost dirs (all files tombstoned) never surface in search results.
    let working_dirs: Vec<&DirItem> = if parsed_query.constraints.is_empty() {
        dirs.iter().filter(|d| !d.is_deleted()).collect()
    } else {
        match apply_constraints(dirs, &parsed_query.constraints, arena, overflow_arena) {
            Some(filtered) if !filtered.is_empty() => {
                filtered.into_iter().filter(|d| !d.is_deleted()).collect()
            }
            Some(_) => return (vec![], vec![], 0),
            None => dirs.iter().filter(|d| !d.is_deleted()).collect(),
        }
    };

    let fuzzy_parts: &[&str] = match &parsed_query.fuzzy_query {
        FuzzyQuery::Text(t) if t.len() >= 2 => std::slice::from_ref(t),
        FuzzyQuery::Parts(parts) if !parts.is_empty() => parts.as_slice(),
        _ => {
            return score_dirs_by_frecency(&working_dirs, context);
        }
    };

    let valid_parts: SmallVec<[&str; 4]> = fuzzy_parts
        .iter()
        .copied()
        .filter(|p| p.len() >= 2)
        .collect();

    if valid_parts.is_empty() {
        return score_dirs_by_frecency(&working_dirs, context);
    }

    let has_uppercase = valid_parts
        .iter()
        .any(|p| p.chars().any(|c| c.is_uppercase()));

    let options = neo_frizbee::Config {
        max_typos: Some(context.max_typos),
        sort: neo_frizbee::SortStrategy::Unsorted,
        scoring: Scoring {
            capitalization_bonus: if has_uppercase { 8 } else { 0 },
            matching_case_bonus: if has_uppercase { 4 } else { 0 },
            ..Default::default()
        },
        ..Default::default()
    };

    let path_matches = match_fuzzy_parts_dirs(
        fuzzy_parts,
        &working_dirs,
        &options,
        context.max_threads,
        arena,
        overflow_arena,
    );

    let main_needle = valid_parts[0].as_bytes();
    let main_needle_len = main_needle.len() as u16;

    let mut dirname_buf = String::with_capacity(32);

    let results: Vec<(&DirItem, Score)> = path_matches
        .into_iter()
        .map(|path_match| {
            let dir = working_dirs[path_match.index as usize];
            let dir_arena = if dir.is_overflow() {
                overflow_arena
            } else {
                arena
            };
            let base_score = path_match.score as i32;
            let frecency_boost = base_score.saturating_mul(dir.max_access_frecency()) / 500;

            let mut rel_buf = String::with_capacity(128);
            dir.path.write_to_string(dir_arena, &mut rel_buf);
            let needle_str = std::str::from_utf8(main_needle).unwrap_or("");
            let match_type = classify_match_type(needle_str, &rel_buf);

            let total = base_score.saturating_add(frecency_boost);

            let score = Score {
                total,
                base_score,
                frecency_boost,
                current_file_penalty: 0,
                exact_match: path_match.exact,
                match_type,
            };

            (dir, score)
        })
        .collect();

    sort_and_paginate_dirs(results, context)
}

fn score_dirs_by_frecency<'a>(
    dirs: &[&'a DirItem],
    context: &ScoringContext,
) -> (Vec<&'a DirItem>, Vec<Score>, usize) {
    let results: Vec<(&DirItem, Score)> = dirs
        .iter()
        .map(|&dir| {
            let score = Score {
                total: dir.max_access_frecency(),
                frecency_boost: dir.max_access_frecency(),
                match_type: MatchType::Full,
                ..Default::default()
            };

            (dir, score)
        })
        .collect();

    sort_and_paginate_dirs(results, context)
}

/// Sort dir results by total score (descending) and apply pagination.
fn sort_and_paginate_dirs<'a>(
    mut results: Vec<(&'a DirItem, Score)>,
    context: &ScoringContext,
) -> (Vec<&'a DirItem>, Vec<Score>, usize) {
    let total_matched = results.len();
    if total_matched == 0 {
        return (vec![], vec![], 0);
    }

    let offset = context.pagination.offset;
    let limit = if context.pagination.limit > 0 {
        context.pagination.limit
    } else {
        total_matched
    };

    if offset >= total_matched {
        return (vec![], vec![], total_matched);
    }

    let items_needed = offset.saturating_add(limit).min(total_matched);
    let use_partial_sort = items_needed < total_matched / 2 && total_matched > 100;

    let compare = |a: &(&DirItem, Score), b: &(&DirItem, Score)| {
        b.1.total
            .cmp(&a.1.total)
            .then_with(|| std::ptr::from_ref(a.0).cmp(&std::ptr::from_ref(b.0)))
    };
    if use_partial_sort {
        results.select_nth_unstable_by(items_needed - 1, compare);
        results.truncate(items_needed);
    }

    sort_with_buffer(&mut results, compare);

    let (items, scores): (Vec<&DirItem>, Vec<Score>) =
        results.into_iter().skip(offset).take(limit).unzip();
    (items, scores, total_matched)
}

fn match_and_score_in_arena<'a>(
    files: &'a [FileItem],
    context: &ScoringContext,
    arena: ArenaPtr,
) -> Vec<(&'a FileItem, Score)> {
    match_and_score_in_arena_inner(files, context, arena)
}

fn classify_match_type(query: &str, relative_path: &str) -> MatchType {
    if query.contains(|c: char| c.is_uppercase()) && relative_path.contains(query) {
        MatchType::Case
    } else if relative_path.to_lowercase().contains(&query.to_lowercase()) {
        MatchType::Full
    } else {
        MatchType::Partial
    }
}

fn match_and_score_in_arena_inner<'a, const WITH_CURRENT_FILE: bool>(
    files: &'a [FileItem],
    context: &ScoringContext,
    arena: ArenaPtr,
) -> Vec<(&'a FileItem, Score)> {
    if files.is_empty() {
        return vec![];
    }

    let Some(working_files) = filter_by_constraints(files, context, arena) else {
        return vec![];
    };
    let Some(fuzzy_parts) = fuzzy_parts(context.query) else {
        // Frecency-only queries are ranked by `rank_by_frecency` before reaching here.
        return frecency_scores(&working_files, context, arena);
    };
    let has_uppercase = fuzzy_parts
        .iter()
        .any(|p| p.chars().any(|c| c.is_uppercase()));
    // Users type `/` regardless of platform. Checking the OS separator alone
    // would miss forward-slash queries on Windows.
    let query_contains_path_separator = fuzzy_parts
        .iter()
        .any(|p| p.contains('/') || p.contains(MAIN_SEPARATOR));

    let options = neo_frizbee::Config {
        max_typos: Some(context.max_typos),
        sort: neo_frizbee::SortStrategy::Unsorted,
        scoring: Scoring {
            capitalization_bonus: if has_uppercase { 8 } else { 0 },
            matching_case_bonus: if has_uppercase { 4 } else { 0 },
            ..Default::default()
        },
        ..Default::default()
    };

    let path_matches = match_fuzzy_parts(
        fuzzy_parts,
        &working_files,
        &options,
        context.max_threads,
        arena,
    );

    let main_needle = fuzzy_parts[0].as_bytes(); // safe
    let main_needle_len = main_needle.len() as u16;

    let mut fallback_indices: Vec<u32> = Vec::new();
    let filename_fallback_matches = if query_contains_path_separator || path_matches.len() > 15_000
    {
        vec![]
    } else {
        let mut fallback_filenames: Vec<Cow<'_, str>> = Vec::new();

        for (i, path_match) in path_matches.iter().enumerate() {
            let file = working_files.index(path_match.index as usize);
            let filename_start = file.filename_offset_in_relative_path() as u16;
            let match_start_approx = path_match.end_col.saturating_sub(main_needle_len - 1);

            if match_start_approx < filename_start {
                fallback_indices.push(i as u32);
                fallback_filenames.push(file.path.filename_cow(arena));
            }
        }

        if fallback_filenames.is_empty() {
            vec![]
        } else {
            // Match on `&str` so frizbee reuses the instantiation its index
            // resolver already emits instead of a separate `Cow<str>` copy.
            let filename_strs: Vec<&str> = fallback_filenames.iter().map(Cow::as_ref).collect();
            neo_frizbee::Matcher::new(fuzzy_parts[0], &options).match_list_parallel(
                &filename_strs,
                if path_matches.len() > 4096 {
                    context.max_threads.div_ceil(2048)
                } else {
                    1
                },
            )
        }
    };

    // path-match index -> position in filename_fallback_matches (u32::MAX = none)
    let mut fallback_by_match: Vec<u32> = Vec::new();
    if !filename_fallback_matches.is_empty() {
        fallback_by_match.resize(path_matches.len(), u32::MAX);
        for (position, m) in filename_fallback_matches.iter().enumerate() {
            fallback_by_match[fallback_indices[m.index as usize] as usize] = position as u32;
        }
    }

    let query_text = fuzzy_parts[0];

    let score_match = |(match_idx, path_match): (usize, &neo_frizbee::Match)| {
        let file_idx = path_match.index as usize;
        let file = working_files.index(file_idx);

        let base_score = path_match.score as i32;
        let frecency_boost = base_score.saturating_mul(file.total_frecency_score()) / 100;
        let current_file_penalty = calculate_current_file_penalty(file, base_score / 4, context, arena);

        let match_type = classify_match_type(query_text, &file.relative_path(arena));

        let total = base_score
            .saturating_add(frecency_boost)
            .saturating_add(current_file_penalty);

        let score = Score {
            total,
            base_score,
            frecency_boost,
            current_file_penalty,
            exact_match: path_match.exact,
            match_type,
        };

        (file, score)
    };

    path_matches
        .iter()
        .enumerate()
        .map(score_match)
        .collect()
}

fn frecency_total(file: &FileItem, context: &ScoringContext, arena: ArenaPtr) -> i32 {
    frecency_score(file, context, arena).total
}

fn frecency_score(file: &FileItem, context: &ScoringContext, arena: ArenaPtr) -> Score {
    let frecency_boost = file.access_frecency_score as i32
        + (file.modification_frecency_score as i32).saturating_mul(4);
    let current_file_penalty = calculate_current_file_penalty(file, frecency_boost, context, arena);
    let total = frecency_boost.saturating_add(current_file_penalty);

    Score {
        total,
        base_score: 0,
        frecency_boost,
        current_file_penalty,
        exact_match: false,
        match_type: MatchType::Full,
    }
}

fn frecency_totals<'a>(
    files: &FileItems<'a>,
    context: &ScoringContext,
    arena: ArenaPtr,
    out: &mut Vec<(&'a FileItem, i32)>,
) {
    let total = |file: &'a FileItem| (file, frecency_total(file, context, arena));
    match files {
        // Small indexes cannot amortize Rayon scheduling for this cheap scoring pass.
        FileItems::All(s) if s.len() >= 32_768 && context.max_threads > 1 => out.par_extend(
            s.par_iter()
                .with_min_len(4096)
                .filter(|f| !f.is_deleted())
                .map(total),
        ),
        FileItems::All(s) => out.extend(s.iter().filter(|f| !f.is_deleted()).map(total)),
        FileItems::Filtered(v) => {
            out.extend(v.iter().copied().filter(|f| !f.is_deleted()).map(total))
        }
    }
}

fn frecency_scores<'a>(
    files: &FileItems<'a>,
    context: &ScoringContext,
    arena: ArenaPtr,
) -> Vec<(&'a FileItem, Score)> {
    let mut totals = Vec::new();
    frecency_totals(files, context, arena, &mut totals);
    totals
        .into_iter()
        .map(|(file, _)| (file, frecency_score(file, context, arena)))
        .collect()
}

#[inline]
fn calculate_current_file_penalty(
    file: &FileItem,
    base_score: i32,
    context: &ScoringContext,
    arena: ArenaPtr,
) -> i32 {
    let mut penalty = 0i32;

    if let Some(current) = context.current_file
        && file.relative_path_eq(arena, current)
    {
        penalty -= base_score;
    }

    penalty
}

/// Sorts elements by total score (descending) and returns the requested page.
/// Always returns results in descending order (best scores first).
/// The UI layer handles rendering order based on prompt position.
#[tracing::instrument(skip_all, level = tracing::Level::DEBUG)]
fn sort_and_paginate<'a, S>(
    mut results: Vec<(&'a FileItem, S)>,
    context: &ScoringContext,
    total: impl Fn(&S) -> i32,
) -> (Vec<&'a FileItem>, Vec<S>, usize) {
    let total_matched = results.len();

    if total_matched == 0 {
        return (vec![], vec![], 0);
    }

    let offset = context.pagination.offset;
    let limit = if context.pagination.limit > 0 {
        context.pagination.limit
    } else {
        total_matched
    };

    // Check if offset is out of bounds
    if offset >= total_matched {
        tracing::warn!(
            offset = offset,
            total_matched = total_matched,
            "Pagination: offset >= total_matched, returning empty"
        );

        return (vec![], vec![], total_matched);
    }

    let items_needed = offset.saturating_add(limit).min(total_matched);
    let compare = |a: &(&FileItem, S), b: &(&FileItem, S)| {
        b.1.match_type
            .cmp(&a.1.match_type)
            .then_with(|| total(&b.1).cmp(&total(&a.1)))
            .then_with(|| b.0.modified.cmp(&a.0.modified))
    };
    // Use partial sort if we need less than half the results and dataset is large
    if items_needed < total_matched / 2 && total_matched > 100 {
        results.select_nth_unstable_by(items_needed - 1, compare);
        results.truncate(items_needed);
    }

    // select nth does not sort the results, we have to sort accordingly anyway
    sort_with_buffer(&mut results, compare);

    let (items, scores): (Vec<&FileItem>, Vec<S>) =
        results.into_iter().skip(offset).take(limit).unzip();
    (items, scores, total_matched)
}

fn match_file_range<F>(
    needle: &str,
    len: usize,
    resolve: &F,
    options: &neo_frizbee::Config,
    max_threads: usize,
) -> Vec<neo_frizbee::Match>
where
    F: Fn(u32, &mut [*const u8; MAX_PATH_CHUNKS]) -> Option<(usize, u16)> + Sync,
{
    // The rayon path below flattens per-worker results and never sorts, so
    // any sorted strategy stays on frizbee's own k-merge implementation.
    let unsorted = matches!(options.sort, neo_frizbee::SortStrategy::Unsorted);
    if len < 32_768 || !unsorted {
        return neo_frizbee::match_range_parallel_resolved(
            needle,
            len,
            resolve,
            options,
            max_threads,
        );
    }
    assert!(
        len <= u32::MAX as usize,
        "too many files for fuzzy matching"
    );
    let mut matcher = neo_frizbee::Matcher::new(needle, options);
    let max_threads = if max_threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2))
            .unwrap_or(1)
    } else {
        max_threads
    };
    let workers = max_threads.max(1).min(len.div_ceil(2000));
    if workers <= 1 {
        return matcher.match_range_resolved(len, resolve);
    }
    let chunk_size = len.div_ceil(workers * 4).clamp(2048, 16_384);
    let next = std::sync::atomic::AtomicUsize::new(0);
    (0..workers)
        .into_par_iter()
        .map(|_| {
            let mut matcher = matcher.clone();
            let mut matches = Vec::new();
            loop {
                let start = next.fetch_add(chunk_size, std::sync::atomic::Ordering::Relaxed);
                if start >= len {
                    break;
                }
                let end = (start + chunk_size).min(len);
                matcher.match_range_resolved_into(start as u32..end as u32, resolve, &mut matches);
            }
            matches
        })
        .collect::<Vec<_>>()
        .into_iter()
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PaginationArgs;
    use fff_query_parser::QueryParser;

    #[test]
    fn longer_second_part_preserves_matches_and_first_part_positions() {
        let paths: Vec<_> = [
            "src/controller.rs",
            "src/contrller.rs",
            "test/src.rs",
            "日本語/controller.tsx",
            "src/utils.rs",
            "src/controller/controller.rs",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let mut files: Vec<_> = paths
            .iter()
            .map(|path| FileItem::new_raw(path.rfind('/').unwrap() as u16 + 1, 0, 0, None, false))
            .collect();
        let (store, strings) =
            crate::simd_path::build_chunked_path_store_from_strings(&paths, &files);
        for (file, path) in files.iter_mut().zip(strings) {
            file.set_path(path);
        }
        let arena = store.as_arena_ptr();
        let working = FileItems::All(&files);
        for parts in [
            ["src", "controller"],
            ["rs", "contrller"],
            ["rs", "日本語"],
            ["no", "absent"],
            ["src", "src/controller.rs"],
        ] {
            for max_typos in [0, 2, 4] {
                let options = neo_frizbee::Config {
                    max_typos: Some(max_typos),
                    sort: neo_frizbee::SortStrategy::Unsorted,
                    ..Default::default()
                };
                let first = match_fuzzy_parts(&parts[..1], &working, &options, 1, arena);
                let mut second_options = options;
                second_options.max_typos = Some(max_typos.min(parts[1].len() as u16));
                let second = match_fuzzy_parts(&parts[1..], &working, &second_options, 1, arena);
                let mut expected: Vec<_> = first
                    .iter()
                    .filter_map(|a| {
                        let b = second.iter().find(|b| b.index == a.index)?;
                        Some((
                            a.index,
                            ((a.score as u32 + b.score as u32) / 2) as u16,
                            a.end_col,
                            a.exact && b.exact,
                        ))
                    })
                    .collect();
                let mut actual: Vec<_> = match_fuzzy_parts(&parts, &working, &options, 4, arena)
                    .iter()
                    .map(|m| (m.index, m.score, m.end_col, m.exact))
                    .collect();
                expected.sort_unstable();
                actual.sort_unstable();
                assert_eq!(actual, expected, "{parts:?}, typos={max_typos}");
            }
        }
    }

    #[test]
    fn fuzzy_parallel_and_sequential_scores_match() {
        let paths: Vec<_> = (0..36_000)
            .map(|index| {
                format!(
                    "src/components/group_{}/controller_é_{index}.tsx",
                    index / 100
                )
            })
            .collect();
        let mut files: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                let mut file = FileItem::new_raw(
                    path.rfind('/').unwrap() as u16 + 1,
                    0,
                    index as u64,
                    Some(git2::Status::WT_MODIFIED),
                    false,
                );
                file.parent_dir_index = (index / 100) as u32;
                file.access_frecency_score = (index % 97) as i16;
                file.modification_frecency_score = (index % 29) as i16;
                file.git_recency_score = (index % 13) as i16;
                file.set_deleted(index % 97 == 0);
                file
            })
            .collect();
        let (store, strings) =
            crate::simd_path::build_chunked_path_store_from_strings(&paths, &files);
        for (file, path) in files.iter_mut().zip(strings) {
            file.set_path(path);
        }
        let arena = store.as_arena_ptr();
        let parser = QueryParser::default();
        for text in [
            "mo",
            "controller",
            "src controller",
            "group_0/controller_é_1.tsx",
            "*.tsx",
        ] {
            let query = parser.parse(text);
            for current_file in [None, Some(paths[1].as_str())] {
                let mut context = ScoringContext {
                    query: &query,
                    max_threads: 4,
                    max_typos: 2,
                    project_path: None,
                    current_file,
                    pagination: PaginationArgs::default(),
                };
                let mut parallel = match_and_score_in_arena(&files, &context, arena);
                context.max_threads = 1;
                let mut sequential = match_and_score_in_arena(&files, &context, arena);
                parallel.sort_unstable_by_key(|(file, _)| file.modified);
                sequential.sort_unstable_by_key(|(file, _)| file.modified);
                assert_eq!(parallel.len(), sequential.len(), "{text}");
                for ((a, a_score), (b, b_score)) in parallel.iter().zip(&sequential) {
                    assert!(std::ptr::eq(*a, *b));
                    assert_eq!(format!("{a_score:?}"), format!("{b_score:?}"), "{text}");
                }
                let (parallel_items, parallel_scores, _) =
                    sort_and_paginate(parallel, &context, |score| score.total);
                let (sequential_items, sequential_scores, _) =
                    sort_and_paginate(sequential, &context, |score| score.total);
                assert_eq!(
                    parallel_items
                        .iter()
                        .map(|f| f.modified)
                        .collect::<Vec<_>>(),
                    sequential_items
                        .iter()
                        .map(|f| f.modified)
                        .collect::<Vec<_>>()
                );
                assert_eq!(
                    format!("{parallel_scores:?}"),
                    format!("{sequential_scores:?}")
                );
            }
        }
    }

    #[test]
    fn frecency_parallel_and_sequential_scores_match() {
        let files: Vec<_> = (0..33_000)
            .map(|index| {
                let mut file =
                    FileItem::new_raw(0, 0, index, Some(git2::Status::WT_MODIFIED), false);
                file.access_frecency_score = (index % 97) as i16;
                file.modification_frecency_score = (index % 29) as i16;
                file.git_recency_score = (index % 13) as i16;
                file.set_deleted(index % 7 == 0);
                file
            })
            .collect();
        let parser = QueryParser::default();
        let query = parser.parse("");
        let mut context = ScoringContext {
            query: &query,
            max_threads: 4,
            max_typos: 0,
            project_path: None,
            current_file: None,
            pagination: PaginationArgs {
                offset: 0,
                limit: 50,
            },
        };
        for count in [0, 1, 1000, 33_000] {
            let items = FileItems::All(&files[..count]);
            context.max_threads = 4;
            let parallel = frecency_scores(&items, &context, ArenaPtr::null());
            context.max_threads = 1;
            let sequential = frecency_scores(&items, &context, ArenaPtr::null());
            assert_eq!(parallel.len(), count - count.div_ceil(7));
            assert_eq!(parallel.len(), sequential.len());
            for ((a, a_score), (b, b_score)) in parallel.iter().zip(&sequential) {
                assert!(std::ptr::eq(*a, *b));
                assert!(!a.is_deleted());
                assert_eq!(a_score.total, b_score.total);
                assert_eq!(a_score.frecency_boost, b_score.frecency_boost);
            }

            // Keyed ranking must produce the same page as ranking full scores.
            context.max_threads = 4;
            let (expected_items, expected_scores, expected_total) =
                sort_and_paginate(parallel, &context, |score| score.total);
            let (items, scores, total) = rank_by_frecency(
                &files[..count],
                &context,
                count / 2,
                ArenaPtr::null(),
                ArenaPtr::null(),
            );
            assert_eq!(total, expected_total);
            assert_eq!(items.len(), expected_items.len());
            for ((a, a_score), (b, b_score)) in items
                .iter()
                .zip(&scores)
                .zip(expected_items.iter().zip(&expected_scores))
            {
                assert!(std::ptr::eq(*a, *b));
                assert_eq!(a_score.total, b_score.total);
                assert_eq!(a_score.match_type, MatchType::Full);
            }
        }
    }

    #[test]
    fn pagination_matches_full_ranking() {
        let files: Vec<_> = (0..257)
            .map(|i| FileItem::new_raw(0, 0, i as u64, None, false))
            .collect();
        let dirs: Vec<_> = (0..257)
            .map(|_| DirItem::new(crate::simd_path::ChunkedString::empty(), 0))
            .collect();
        let parser = QueryParser::default();
        let query = parser.parse("");
        for count in [0, 1, 3, 100, 101, 257] {
            let results: Vec<_> = files[..count]
                .iter()
                .enumerate()
                .map(|(index, file)| {
                    (
                        file,
                        Score {
                            total: (index * 73 % 257) as i32,
                            ..Default::default()
                        },
                    )
                })
                .collect();
            let mut expected = results.clone();
            expected.sort_by(|a, b| {
                b.1.total
                    .cmp(&a.1.total)
                    .then_with(|| b.0.modified.cmp(&a.0.modified))
            });
            for offset in [0, 1, count / 2, count, usize::MAX] {
                for limit in [0, 1, 2, 50, count, usize::MAX] {
                    let context = ScoringContext {
                        query: &query,
                        max_threads: 1,
                        max_typos: 0,
                        project_path: None,
                        current_file: None,
                        pagination: PaginationArgs { offset, limit },
                    };
                    let expected_page: Vec<_> = expected
                        .iter()
                        .skip(offset)
                        .take(if limit == 0 { count } else { limit })
                        .map(|(_, score)| score.total)
                        .collect();
                    let (items, scores, total) =
                        sort_and_paginate(results.clone(), &context, |s| s.total);
                    assert_eq!(total, count);
                    assert_eq!(items.len(), expected_page.len());
                    assert_eq!(
                        scores.iter().map(|score| score.total).collect::<Vec<_>>(),
                        expected_page
                    );
                    let dir_results = results
                        .iter()
                        .enumerate()
                        .map(|(index, (_, score))| (&dirs[index], score.clone()))
                        .collect();
                    let (items, scores, total) = sort_and_paginate_dirs(dir_results, &context);
                    assert_eq!(total, count);
                    assert_eq!(items.len(), expected_page.len());
                    assert_eq!(
                        scores.iter().map(|score| score.total).collect::<Vec<_>>(),
                        expected_page
                    );
                }
            }
        }
    }

    #[test]
    fn tied_scores_paginate_deterministically() {
        let files: Vec<_> = (0..3000)
            .map(|_| FileItem::new_raw(0, 0, 1, None, false))
            .collect();
        let parser = QueryParser::default();
        let query = parser.parse("");
        let page = |shuffle: usize, offset: usize| {
            let mut results: Vec<_> = files.iter().map(|file| (file, Score::default())).collect();
            results.rotate_left(shuffle);
            results.reverse();
            let context = ScoringContext {
                query: &query,
                max_threads: 1,
                max_typos: 0,
                project_path: None,
                current_file: None,
                pagination: PaginationArgs { offset, limit: 8 },
            };
            sort_and_paginate(results, &context, |s| s.total).0
        };
        for offset in [0, 8, 16] {
            let expected: Vec<_> = files[offset..offset + 8].iter().collect();
            for shuffle in [0, 7, 1500, 2999] {
                assert!(
                    page(shuffle, offset)
                        .iter()
                        .zip(&expected)
                        .all(|(a, b)| std::ptr::eq(*a, *b)),
                    "offset={offset} shuffle={shuffle}"
                );
            }
        }
    }

    #[test]
    fn merge_offsets_reuses_storage() {
        let mut ranges: SmallVec<[(u32, u32); 4]> = smallvec::smallvec![
            (20, 25),
            (2, 6),
            (0, 3),
            (6, 10),
            (11, 11),
            (24, 30),
            (40, 45),
        ];
        let storage = ranges.as_mut_ptr();
        let merged = merge_byte_offsets(ranges);
        assert_eq!(merged.as_slice(), &[(0, 10), (20, 30), (40, 45)]);
        assert_eq!(merged.as_ptr(), storage);
    }

    fn make_test_files(specs: &[(&str, i32, u64)]) -> (Vec<(FileItem, Score)>, ArenaPtr) {
        let path_strings: Vec<String> = specs.iter().map(|(p, _, _)| p.to_string()).collect();
        let items: Vec<FileItem> = specs
            .iter()
            .map(|(p, _, _)| {
                let fname = p.rfind(std::path::is_separator).map(|i| i + 1).unwrap_or(0) as u16;
                FileItem::new_raw(fname, 0, 0, None, false)
            })
            .collect();
        let (store, strings) =
            crate::simd_path::build_chunked_path_store_from_strings(&path_strings, &items);
        let arena = store.as_arena_ptr();
        let result: Vec<(FileItem, Score)> = specs
            .iter()
            .enumerate()
            .map(|(i, &(_, score, modified))| {
                let filename_start = path_strings[i]
                    .rfind(std::path::is_separator)
                    .map(|j| j + 1)
                    .unwrap_or(0) as u16;
                let mut file = FileItem::new_raw(filename_start, 0, modified, None, false);
                file.set_path(strings[i].clone());
                let score_obj = Score {
                    total: score,
                    base_score: score,
                    current_file_penalty: 0,
                    frecency_boost: 0,
                    exact_match: false,
                    match_type: MatchType::Full,
                };
                (file, score_obj)
            })
            .collect();
        std::mem::forget(store);
        (result, arena)
    }

    #[test]
    fn test_partial_sort_descending() {
        // Create test data with known scores
        let (test_data, arena) = make_test_files(&[
            ("file1.rs", 100, 1000),
            ("file2.rs", 200, 2000),
            ("file3.rs", 50, 3000),
            ("file4.rs", 300, 4000),
            ("file5.rs", 150, 5000),
            ("file6.rs", 250, 6000),
            ("file7.rs", 80, 7000),
            ("file8.rs", 180, 8000),
            ("file9.rs", 120, 9000),
            ("file10.rs", 90, 10000),
        ]);

        // Convert to references like the actual function uses
        let results: Vec<(&FileItem, Score)> = test_data
            .iter()
            .map(|(file, score)| (file, score.clone()))
            .collect();

        let query_str = "test";
        let parser = QueryParser::default();
        let query = parser.parse(query_str);
        let context = ScoringContext {
            query: &query,
            max_threads: 1,
            max_typos: 2,
            current_file: None,
            project_path: None,
            pagination: PaginationArgs {
                offset: 0,
                limit: 0,
            },
        };

        // Test with full sort - returns all results sorted descending
        let (items, scores, total) = sort_and_paginate(results.clone(), &context, |s| s.total);

        // Should return all 10 items sorted by score descending
        assert_eq!(total, 10);
        assert_eq!(scores.len(), 10);
        assert_eq!(scores[0].total, 300, "First should be highest score");
        assert_eq!(scores[1].total, 250, "Second should be second highest");
        assert_eq!(scores[2].total, 200, "Third should be third highest");

        // Verify the files match
        assert_eq!(items[0].relative_path(arena), "file4.rs");
        assert_eq!(items[1].relative_path(arena), "file6.rs");
        assert_eq!(items[2].relative_path(arena), "file2.rs");
    }

    #[test]
    fn test_partial_sort_with_same_scores() {
        // Test tiebreaker with modified time
        let (test_data, _arena) = make_test_files(&[
            ("file1.rs", 100, 5000), // Same score, older
            ("file2.rs", 100, 8000), // Same score, newer
            ("file3.rs", 100, 3000), // Same score, oldest
            ("file4.rs", 200, 1000),
            ("file5.rs", 200, 9000), // Higher score, newest
        ]);

        let results: Vec<(&FileItem, Score)> = test_data
            .iter()
            .map(|(file, score)| (file, score.clone()))
            .collect();

        let query_str = "test";
        let parser = QueryParser::default();
        let query = parser.parse(query_str);
        let context = ScoringContext {
            query: &query,
            max_threads: 1,
            max_typos: 2,
            current_file: None,
            project_path: None,

            pagination: PaginationArgs {
                offset: 0,
                limit: 0,
            },
        };

        let (items, scores, _) = sort_and_paginate(results, &context, |s| s.total);

        // Should return all 5 items sorted: 200(9000), 200(1000), 100(8000), 100(5000), 100(3000)
        assert_eq!(scores.len(), 5);
        assert_eq!(scores[0].total, 200);
        assert_eq!(items[0].modified, 9000, "First 200 should be newest");
        assert_eq!(scores[1].total, 200);
        assert_eq!(items[1].modified, 1000, "Second 200 should be older");
        assert_eq!(scores[2].total, 100);
        assert_eq!(items[2].modified, 8000, "First 100 should be newest");
        assert_eq!(scores[3].total, 100);
        assert_eq!(items[3].modified, 5000);
        assert_eq!(scores[4].total, 100);
        assert_eq!(items[4].modified, 3000, "Last 100 should be oldest");
    }

    #[test]
    fn test_no_partial_sort_for_small_results() {
        // When results.len() <= threshold, should use regular sort
        let (test_data, arena) = make_test_files(&[
            ("file1.rs", 100, 1000),
            ("file2.rs", 200, 2000),
            ("file3.rs", 50, 3000),
        ]);

        let results: Vec<(&FileItem, Score)> = test_data
            .iter()
            .map(|(file, score)| (file, score.clone()))
            .collect();

        let query_str = "test";
        let parser = QueryParser::default();
        let query = parser.parse(query_str);
        let context = ScoringContext {
            query: &query,
            max_threads: 1,
            max_typos: 2,
            current_file: None,
            project_path: None,

            pagination: PaginationArgs {
                offset: 0,
                limit: 0,
            },
        };

        // Returns all results sorted descending
        let (items, scores, _) = sort_and_paginate(results, &context, |s| s.total);

        assert_eq!(scores.len(), 3);
        assert_eq!(scores[0].total, 200);
        assert_eq!(scores[1].total, 100);
        assert_eq!(scores[2].total, 50);
        assert_eq!(items[0].relative_path(arena), "file2.rs");
        assert_eq!(items[1].relative_path(arena), "file1.rs");
        assert_eq!(items[2].relative_path(arena), "file3.rs");
    }
}

#[cfg(test)]
mod typo_resistance_tests {
    use super::*;
    use crate::types::PaginationArgs;
    use fff_query_parser::QueryParser;

    fn make_files(paths: &[&str]) -> (Vec<FileItem>, ArenaPtr) {
        let path_strings: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        let items: Vec<FileItem> = paths
            .iter()
            .map(|p| {
                let fname = p.rfind(std::path::is_separator).map(|i| i + 1).unwrap_or(0) as u16;
                FileItem::new_raw(fname, 0, 0, None, false)
            })
            .collect();
        let (store, strings) =
            crate::simd_path::build_chunked_path_store_from_strings(&path_strings, &items);
        let arena = store.as_arena_ptr();
        let mut result: Vec<FileItem> = items;
        for (i, file) in result.iter_mut().enumerate() {
            file.set_path(strings[i].clone());
        }
        std::mem::forget(store);
        (result, arena)
    }

    fn search_with_typos(
        files: &[FileItem],
        query: &str,
        arena: ArenaPtr,
        max_typos: u16,
    ) -> Vec<String> {
        let parser = QueryParser::default();
        let parsed = parser.parse(query);
        let ctx = ScoringContext {
            query: &parsed,
            max_threads: 1,
            max_typos,
            current_file: None,
            project_path: None,
            pagination: PaginationArgs {
                offset: 0,
                limit: 100,
            },
        };
        let (items, _, _) = fuzzy_match_and_score_files(files, &ctx, files.len(), arena, arena);
        items.iter().map(|f| f.relative_path(arena)).collect()
    }

    #[test]
    fn test_typo_resistant_long_query() {
        let (files, arena) = make_files(&[
            "src/pricing/bid_comparison_supplier_part_cost_modifiers.rs",
            "src/pricing/bid_evaluation_handler.rs",
            "src/pricing/supplier_contract_terms.rs",
            "src/models/purchase_order.rs",
            "src/models/inventory_item.rs",
            "src/controllers/user_controller.rs",
            "src/controllers/admin_controller.rs",
            "src/services/notification_service.rs",
            "src/services/email_dispatcher.rs",
            "src/utils/string_helpers.rs",
            "src/utils/date_formatter.rs",
            "src/db/migration_runner.rs",
            "src/db/connection_pool.rs",
            "src/config/app_settings.rs",
            "src/config/feature_flags.rs",
        ]);

        // Exact substring — must always match
        let results = search_with_typos(&files, "bid_comparison", arena, 6);
        assert!(
            results
                .iter()
                .any(|p| p.contains("bid_comparison_supplier_part_cost_modifiers")),
            "exact substring 'bid_comparison' should match, got: {results:?}"
        );

        // Concatenated query with typos — the real-world scenario.
        // "bidcomparsionsupplierpartcostmodfiers" is a smushed version of
        // "bid_comparison_supplier_part_cost_modifiers" with 2 typos
        // (comparison → comparison, modifiers → modifiers).
        let results = search_with_typos(&files, "bidcomparsionsupplierpartcostmodfiers", arena, 6);
        assert!(
            results
                .iter()
                .any(|p| p.contains("bid_comparison_supplier_part_cost_modifiers")),
            "typo query 'bidcomparsionsupplierpartcostmodfiers' should match, got: {results:?}"
        );

        // Shorter typo query
        let results = search_with_typos(&files, "bidcomp", arena, 4);
        assert!(
            results.iter().any(|p| p.contains("bid_comparison")),
            "'bidcomp' should match bid_comparison file, got: {results:?}"
        );

        // Query with missing underscores
        let results = search_with_typos(&files, "supplierpartcost", arena, 6);
        assert!(
            results
                .iter()
                .any(|p| p.contains("bid_comparison_supplier_part_cost_modifiers")),
            "'supplierpartcost' should match, got: {results:?}"
        );
    }
}

#[cfg(test)]
mod constraint_only_query_tests {
    use super::*;
    use crate::types::PaginationArgs;
    use fff_query_parser::QueryParser;

    #[test]
    fn constraint_only_git_status_query_returns_filtered_files() {
        let paths = ["modified.rs", "clean.rs"];
        let path_strings: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        let items: Vec<FileItem> = paths
            .iter()
            .map(|p| {
                let fname = p.rfind(std::path::is_separator).map(|i| i + 1).unwrap_or(0) as u16;
                FileItem::new_raw(fname, 0, 0, None, false)
            })
            .collect();
        let (store, strings) =
            crate::simd_path::build_chunked_path_store_from_strings(&path_strings, &items);
        let arena = store.as_arena_ptr();
        let mut files: Vec<FileItem> = items;
        for (i, file) in files.iter_mut().enumerate() {
            file.set_path(strings[i].clone());
        }
        files[0].git_status = Some(git2::Status::WT_MODIFIED);
        files[1].git_status = Some(git2::Status::CURRENT);
        std::mem::forget(store);

        let parser = QueryParser::default();
        let parsed = parser.parse("git:modified");

        let ctx = ScoringContext {
            query: &parsed,
            max_threads: 1,
            max_typos: 2,
            current_file: None,
            project_path: None,
            pagination: PaginationArgs {
                offset: 0,
                limit: 50,
            },
        };

        let (items, _scores, total_matched) =
            fuzzy_match_and_score_files(&files, &ctx, files.len(), arena, arena);

        assert_eq!(
            total_matched, 1,
            "git:modified constraint-only query should match exactly 1 modified file"
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].relative_path(arena), "modified.rs");
    }

    #[test]
    fn constraint_only_status_modified_alias_returns_filtered_files() {
        let paths = ["a.rs", "b.rs"];
        let path_strings: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        let items: Vec<FileItem> = paths
            .iter()
            .map(|p| {
                let fname = p.rfind(std::path::is_separator).map(|i| i + 1).unwrap_or(0) as u16;
                FileItem::new_raw(fname, 0, 0, None, false)
            })
            .collect();
        let (store, strings) =
            crate::simd_path::build_chunked_path_store_from_strings(&path_strings, &items);
        let arena = store.as_arena_ptr();
        let mut files: Vec<FileItem> = items;
        for (i, file) in files.iter_mut().enumerate() {
            file.set_path(strings[i].clone());
        }
        files[0].git_status = Some(git2::Status::WT_MODIFIED);
        files[1].git_status = Some(git2::Status::CURRENT);
        std::mem::forget(store);

        let parser = QueryParser::default();
        let parsed = parser.parse("status:modified");

        let ctx = ScoringContext {
            query: &parsed,
            max_threads: 1,
            max_typos: 2,
            current_file: None,
            project_path: None,
            pagination: PaginationArgs {
                offset: 0,
                limit: 50,
            },
        };

        let (items, _scores, total_matched) =
            fuzzy_match_and_score_files(&files, &ctx, files.len(), arena, arena);

        assert_eq!(total_matched, 1);
        assert_eq!(items[0].relative_path(arena), "a.rs");
    }
}
