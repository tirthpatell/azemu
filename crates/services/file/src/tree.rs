//! Walking the tree of a share.

use azemu_core::{RequestCtx, Response};

use crate::errors;
use crate::state::{Dir, File, Node};

/// Why a path did not lead anywhere.
#[derive(Debug, PartialEq, Eq)]
pub enum Miss {
    /// A directory on the way is not there (404 `ParentNotFound`).
    Parent,
    /// The last name is not there (404 `ResourceNotFound`).
    Itself,
}

impl Miss {
    pub fn response(&self, ctx: &RequestCtx) -> Response {
        match self {
            Miss::Parent => errors::parent_not_found(ctx),
            Miss::Itself => errors::resource_not_found(ctx),
        }
    }
}

/// The directory at the end of `dirs`, starting at `root`.
pub fn walk<'a>(root: &'a Dir, dirs: &[String]) -> Option<&'a Dir> {
    let mut cur = root;
    for n in dirs {
        cur = cur.dir(n)?;
    }
    Some(cur)
}

pub fn walk_mut<'a>(root: &'a mut Dir, dirs: &[String]) -> Option<&'a mut Dir> {
    let mut cur = root;
    for n in dirs {
        cur = cur.dir_mut(n)?;
    }
    Some(cur)
}

/// The directory a path names (the root for an empty path).
pub fn dir_at<'a>(root: &'a Dir, path: &[String]) -> Result<&'a Dir, Miss> {
    let Some((last, parents)) = path.split_last() else { return Ok(root) };
    let parent = walk(root, parents).ok_or(Miss::Parent)?;
    parent.dir(last).ok_or(Miss::Itself)
}

pub fn file_at<'a>(root: &'a Dir, path: &[String]) -> Result<&'a File, Miss> {
    let (last, parents) = path.split_last().ok_or(Miss::Itself)?;
    let parent = walk(root, parents).ok_or(Miss::Parent)?;
    parent.file(last).ok_or(Miss::Itself)
}

/// What a path names, a directory or a file.
pub fn node_at<'a>(root: &'a Dir, path: &[String]) -> Result<&'a Node, Miss> {
    let (last, parents) = path.split_last().ok_or(Miss::Itself)?;
    let parent = walk(root, parents).ok_or(Miss::Parent)?;
    parent.children.get(&last.to_lowercase()).ok_or(Miss::Itself)
}

/// The size of everything under `dir`, in bytes.
pub fn usage(dir: &Dir) -> u64 {
    dir.children
        .values()
        .map(|n| match n {
            Node::Dir(d) => usage(d),
            Node::File(f) => f.size,
        })
        .sum()
}
