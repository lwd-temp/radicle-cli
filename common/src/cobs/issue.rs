use std::collections::{HashMap, HashSet};
use std::convert::{Infallible, TryFrom};
use std::ops::ControlFlow;
use std::str::FromStr;

use automerge::{Automerge, AutomergeError, ObjType, ScalarValue, Value};
use lazy_static::lazy_static;
use nonempty::NonEmpty;

use librad::collaborative_objects::{
    CollaborativeObjects, EntryContents, NewObjectSpec, ObjectId, TypeName, UpdateObjectSpec,
};
use librad::git::identities::local::LocalIdentity;
use librad::git::Storage;
use librad::git::Urn;
use librad::paths::Paths;

lazy_static! {
    pub static ref TYPENAME: TypeName = FromStr::from_str("xyz.radicle.issue").unwrap();
    pub static ref SCHEMA: serde_json::Value =
        serde_json::from_slice(include_bytes!("issue.json")).unwrap();
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Create error: {0}")]
    Create(String),

    #[error("Retrieve error: {0}")]
    Retrieve(String),

    #[error(transparent)]
    Automerge(#[from] AutomergeError),
}

#[derive(Debug, PartialEq, Eq, Hash, Copy, Clone)]
pub struct Reaction {
    pub emoji: char,
}

impl Reaction {
    pub fn new(emoji: char) -> Result<Self, Infallible> {
        Ok(Self { emoji })
    }
}

impl FromStr for Reaction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut chars = s.chars();
        let first = chars.next().ok_or(String::new())?;

        // Reactions should not consist of more than a single emoji.
        if chars.next().is_some() {
            return Err(String::new());
        }
        Ok(Reaction::new(first).unwrap())
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Label(String);

impl Label {
    pub fn new(name: impl Into<String>) -> Result<Self, Infallible> {
        Ok(Self(name.into()))
    }

    pub fn name(&self) -> &str {
        self.0.as_str()
    }
}

impl From<Label> for String {
    fn from(Label(name): Label) -> Self {
        name
    }
}

#[derive(Debug)]
pub struct Comment {
    pub author: Urn,
    pub body: String,
    pub reactions: HashMap<Reaction, usize>,
}

pub fn author(val: Value) -> Result<Urn, AutomergeError> {
    let author = val.into_string().unwrap();
    let author = Urn::from_str(&author).unwrap();

    Ok(author)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Closed,
}

impl From<State> for ScalarValue {
    fn from(state: State) -> Self {
        match state {
            State::Open => ScalarValue::from("open"),
            State::Closed => ScalarValue::from("closed"),
        }
    }
}

impl<'a> TryFrom<Value<'a>> for State {
    type Error = &'static str;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let state = value.to_str().ok_or("value isn't a string")?;

        match state {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            _ => Err("invalid state name"),
        }
    }
}

#[derive(Debug)]
pub struct Issue {
    pub author: Urn,
    pub title: String,
    pub state: State,
    pub comments: NonEmpty<Comment>,
    pub labels: HashSet<Label>,
}

impl Issue {
    pub fn author(&self) -> &Urn {
        &self.author
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn description(&self) -> &str {
        &self.comments.head.body
    }

    pub fn comments(&self) -> &[Comment] {
        &self.comments.tail
    }

    pub fn reactions(&self) -> &HashMap<Reaction, usize> {
        &self.comments.head.reactions
    }

    pub fn labels(&self) -> &HashSet<Label> {
        &self.labels
    }
}

impl TryFrom<Automerge> for Issue {
    type Error = AutomergeError;

    fn try_from(doc: Automerge) -> Result<Self, Self::Error> {
        let (_obj, obj_id) = doc.get(automerge::ObjId::Root, "issue")?.unwrap();
        let (title, _) = doc.get(&obj_id, "title")?.unwrap();
        let (comments, comments_id) = doc.get(&obj_id, "comments")?.unwrap();
        let (author, _) = doc.get(&obj_id, "author")?.unwrap();
        let (state, _) = doc.get(&obj_id, "state")?.unwrap();
        let (labels, labels_id) = doc.get(&obj_id, "labels")?.unwrap();

        assert_eq!(comments.to_objtype(), Some(ObjType::List));
        assert_eq!(labels.to_objtype(), Some(ObjType::Map));

        // Comments.
        let mut comments = Vec::new();
        for i in 0..doc.length(&comments_id) {
            let (_val, comment_id) = doc.get(&comments_id, i as usize)?.unwrap();
            let (author, _) = doc.get(&comment_id, "author")?.unwrap();
            let (body, _) = doc.get(&comment_id, "body")?.unwrap();
            let (_, reactions_id) = doc.get(&comment_id, "reactions")?.unwrap();

            let author = self::author(author)?;
            let body = body.into_string().unwrap();

            let mut reactions: HashMap<_, usize> = HashMap::new();
            for reaction in doc.keys(&reactions_id) {
                let key = Reaction::from_str(&reaction).unwrap();
                let count = reactions.entry(key).or_default();

                *count += 1;
            }

            comments.push(Comment {
                author,
                body,
                reactions,
            });
        }

        // Labels.
        let mut labels = HashSet::new();
        for key in doc.keys(&labels_id) {
            let label = Label::new(key).unwrap();
            labels.insert(label);
        }

        let author = self::author(author)?;
        let comments = NonEmpty::from_vec(comments).unwrap();
        let state = State::try_from(state).unwrap();

        Ok(Self {
            title: title.into_string().unwrap(),
            state,
            author,
            comments,
            labels,
        })
    }
}

pub struct Issues<'a> {
    store: CollaborativeObjects<'a>,
    whoami: LocalIdentity,
}

impl<'a> Issues<'a> {
    pub fn new(whoami: LocalIdentity, paths: &Paths, storage: &'a Storage) -> Result<Self, Error> {
        let store = storage.collaborative_objects(Some(paths.cob_cache_dir().to_path_buf()));

        Ok(Self { store, whoami })
    }

    pub fn create(&self, project: &Urn, title: &str, description: &str) -> Result<ObjectId, Error> {
        let author = self.whoami.urn();
        let history = events::create(&author, title, description)?;
        let cob = self
            .store
            .create(
                &self.whoami,
                project,
                NewObjectSpec {
                    schema_json: SCHEMA.clone(),
                    typename: TYPENAME.clone(),
                    message: Some("Create issue".to_owned()),
                    history,
                },
            )
            .map_err(|e| Error::Create(e.to_string()))?;

        Ok(*cob.id())
    }

    pub fn comment(
        &self,
        project: &Urn,
        issue_id: &ObjectId,
        body: &str,
    ) -> Result<ObjectId, Error> {
        let author = self.whoami.urn();
        let mut issue = self.get_raw(project, issue_id)?.unwrap();
        let changes = events::comment(&mut issue, &author, body)?;
        let cob = self
            .store
            .update(
                &self.whoami,
                project,
                UpdateObjectSpec {
                    object_id: *issue_id,
                    typename: TYPENAME.clone(),
                    message: Some("Add comment".to_owned()),
                    changes,
                },
            )
            .unwrap();

        Ok(*cob.id()) // TODO: Return something other than doc id.
    }

    pub fn close(&self, project: &Urn, issue_id: &ObjectId) -> Result<(), Error> {
        let author = self.whoami.urn();
        let mut issue = self.get_raw(project, issue_id)?.unwrap();
        let changes = events::lifecycle(&mut issue, &author, State::Closed)?;
        let _cob = self
            .store
            .update(
                &self.whoami,
                project,
                UpdateObjectSpec {
                    object_id: *issue_id,
                    typename: TYPENAME.clone(),
                    message: Some("Add comment".to_owned()),
                    changes,
                },
            )
            .unwrap();

        Ok(())
    }

    pub fn label(&self, project: &Urn, issue_id: &ObjectId, labels: &[Label]) -> Result<(), Error> {
        let author = self.whoami.urn();
        let mut issue = self.get_raw(project, issue_id)?.unwrap();
        let changes = events::label(&mut issue, &author, labels)?;
        let _cob = self
            .store
            .update(
                &self.whoami,
                project,
                UpdateObjectSpec {
                    object_id: *issue_id,
                    typename: TYPENAME.clone(),
                    message: Some("Add label".to_owned()),
                    changes,
                },
            )
            .unwrap();

        Ok(())
    }

    pub fn react(
        &self,
        project: &Urn,
        issue_id: &ObjectId,
        comment_ix: usize,
        reaction: Reaction,
    ) -> Result<(), Error> {
        let author = self.whoami.urn();
        let mut issue = self.get_raw(project, issue_id)?.unwrap();
        let changes = events::react(&mut issue, comment_ix, &author, &[reaction])?;
        let _cob = self
            .store
            .update(
                &self.whoami,
                project,
                UpdateObjectSpec {
                    object_id: *issue_id,
                    typename: TYPENAME.clone(),
                    message: Some("Add comment".to_owned()),
                    changes,
                },
            )
            .unwrap();

        Ok(())
    }

    pub fn get(&self, project: &Urn, id: &ObjectId) -> Result<Option<Issue>, Error> {
        let cob = self
            .store
            .retrieve(project, &TYPENAME, id)
            .map_err(|e| Error::Retrieve(e.to_string()))?;

        let cob = if let Some(cob) = cob {
            cob
        } else {
            return Ok(None);
        };

        let doc = cob.history().traverse(Automerge::new(), |mut doc, entry| {
            match entry.contents() {
                EntryContents::Automerge(bytes) => {
                    match automerge::Change::from_bytes(bytes.clone()) {
                        Ok(change) => {
                            doc.apply_changes([change]).ok();
                        }
                        Err(_err) => {
                            // Ignore
                        }
                    }
                }
            }
            ControlFlow::Continue(doc)
        });
        let issue = Issue::try_from(doc)?;

        Ok(Some(issue))
    }

    pub fn get_raw(&self, project: &Urn, id: &ObjectId) -> Result<Option<Automerge>, Error> {
        let cob = self
            .store
            .retrieve(project, &TYPENAME, id)
            .map_err(|e| Error::Retrieve(e.to_string()))?;

        let cob = if let Some(cob) = cob {
            cob
        } else {
            return Ok(None);
        };

        let doc = cob.history().traverse(Vec::new(), |mut doc, entry| {
            match entry.contents() {
                EntryContents::Automerge(bytes) => {
                    doc.extend(bytes);
                }
            }
            ControlFlow::Continue(doc)
        });

        let doc = Automerge::load(&doc)?;

        Ok(Some(doc))
    }
}

mod events {
    use super::*;
    use automerge::{
        transaction::{CommitOptions, Transactable},
        ObjId,
    };

    pub fn create(
        author: &Urn,
        title: &str,
        description: &str,
    ) -> Result<EntryContents, AutomergeError> {
        // TODO: Set actor id of document?
        let mut doc = Automerge::new();
        let _issue = doc
            .transact_with::<_, _, AutomergeError, _, ()>(
                |_| CommitOptions::default().with_message("Create issue".to_owned()),
                |tx| {
                    let issue = tx.put_object(ObjId::Root, "issue", ObjType::Map)?;

                    tx.put(&issue, "title", title)?;
                    tx.put(&issue, "author", author.to_string())?;
                    tx.put(&issue, "state", State::Open)?;
                    tx.put_object(&issue, "labels", ObjType::Map)?;

                    let comments = tx.put_object(&issue, "comments", ObjType::List)?;
                    let comment = tx.insert_object(&comments, 0, ObjType::Map)?;

                    tx.put(&comment, "body", description)?;
                    tx.put(&comment, "author", author.to_string())?;
                    tx.put_object(&comment, "reactions", ObjType::Map)?;

                    Ok(issue)
                },
            )
            .map_err(|failure| failure.error)?
            .result;

        Ok(EntryContents::Automerge(doc.save_incremental()))
    }

    pub fn comment(
        issue: &mut Automerge,
        author: &Urn,
        body: &str,
    ) -> Result<EntryContents, AutomergeError> {
        let _comment = issue
            .transact_with::<_, _, AutomergeError, _, ()>(
                |_| CommitOptions::default().with_message("Add comment".to_owned()),
                |tx| {
                    let (_obj, obj_id) = tx.get(ObjId::Root, "issue")?.unwrap();
                    let (_, comments) = tx.get(&obj_id, "comments")?.unwrap();

                    let length = tx.length(&comments);
                    let comment = tx.insert_object(&comments, length, ObjType::Map)?;

                    tx.put(&comment, "author", author.to_string())?;
                    tx.put(&comment, "body", body)?;
                    tx.put_object(&comment, "labels", ObjType::Map)?;
                    tx.put_object(&comment, "reactions", ObjType::Map)?;

                    Ok(comment)
                },
            )
            .map_err(|failure| failure.error)?
            .result;

        let change = issue.get_last_local_change().unwrap().raw_bytes().to_vec();

        Ok(EntryContents::Automerge(change))
    }

    pub fn lifecycle(
        issue: &mut Automerge,
        _author: &Urn,
        state: State,
    ) -> Result<EntryContents, AutomergeError> {
        issue
            .transact_with::<_, _, AutomergeError, _, ()>(
                |_| CommitOptions::default().with_message("Close issue".to_owned()),
                |tx| {
                    let (_, obj_id) = tx.get(ObjId::Root, "issue")?.unwrap();
                    tx.put(&obj_id, "state", state)?;

                    // TODO: Record who changed the state of the issue.

                    Ok(())
                },
            )
            .map_err(|failure| failure.error)?;

        let change = issue.get_last_local_change().unwrap().raw_bytes().to_vec();

        Ok(EntryContents::Automerge(change))
    }

    pub fn label(
        issue: &mut Automerge,
        _author: &Urn,
        labels: &[Label],
    ) -> Result<EntryContents, AutomergeError> {
        issue
            .transact_with::<_, _, AutomergeError, _, ()>(
                |_| CommitOptions::default().with_message("Close issue".to_owned()),
                |tx| {
                    let (_, obj_id) = tx.get(ObjId::Root, "issue")?.unwrap();
                    let (_, labels_id) = tx.get(&obj_id, "labels")?.unwrap();

                    for label in labels {
                        tx.put(&labels_id, label.name(), true)?;
                    }
                    Ok(())
                },
            )
            .map_err(|failure| failure.error)?;

        let change = issue.get_last_local_change().unwrap().raw_bytes().to_vec();

        Ok(EntryContents::Automerge(change))
    }

    pub fn react(
        issue: &mut Automerge,
        comment_ix: usize,
        author: &Urn,
        reactions: &[Reaction],
    ) -> Result<EntryContents, AutomergeError> {
        issue
            .transact_with::<_, _, AutomergeError, _, ()>(
                |_| CommitOptions::default().with_message("Close issue".to_owned()),
                |tx| {
                    let (_, obj_id) = tx.get(ObjId::Root, "issue")?.unwrap();
                    let (_, comments_id) = tx.get(&obj_id, "comments")?.unwrap();
                    let (_, comment_id) = tx.get(&comments_id, comment_ix)?.unwrap();
                    let (_, reactions_id) = tx.get(&comment_id, "reactions")?.unwrap();

                    for reaction in reactions {
                        let key = reaction.emoji.to_string();
                        let reaction_id = if let Some((_, reaction_id)) =
                            tx.get(&reactions_id, key)?
                        {
                            reaction_id
                        } else {
                            tx.put_object(&reactions_id, reaction.emoji.to_string(), ObjType::Map)?
                        };
                        tx.put(&reaction_id, author.encode_id(), true)?;
                    }

                    Ok(())
                },
            )
            .map_err(|failure| failure.error)?;

        let change = issue.get_last_local_change().unwrap().raw_bytes().to_vec();

        Ok(EntryContents::Automerge(change))
    }
}

#[cfg(test)]
mod test {
    use std::env;
    use std::path::Path;

    use librad::crypto::keystore::crypto::{Pwhash, KDF_PARAMS_TEST};
    use librad::crypto::keystore::pinentry::SecUtf8;
    use librad::git::identities::Project;

    use librad::profile::{Profile, LNK_HOME};

    use super::*;
    use crate::{keys, person, project, test};

    fn setup() -> (Storage, Profile, LocalIdentity, Project) {
        let tempdir = env::temp_dir().join("rad").join("home");
        let home = env::var(LNK_HOME)
            .map(|s| Path::new(&s).to_path_buf())
            .unwrap_or_else(|_| tempdir.to_path_buf());

        env::set_var(LNK_HOME, home);

        let name = "cloudhead";
        let pass = Pwhash::new(SecUtf8::from(test::USER_PASS), *KDF_PARAMS_TEST);
        let (profile, _peer_id) = lnk_profile::create(None, pass.clone()).unwrap();
        let signer = test::signer(&profile, pass).unwrap();
        let storage = keys::storage(&profile, signer.clone()).unwrap();
        let person = person::create(&profile, name, signer, &storage).unwrap();

        person::set_local(&storage, &person);

        let whoami = person::local(&storage).unwrap();
        let payload = project::payload(
            "nakamoto".to_owned(),
            "Bitcoin light-client".to_owned(),
            "master".to_owned(),
        );
        let project = project::create(payload, &storage).unwrap();

        (storage, profile, whoami, project)
    }

    #[test]
    fn test_issue_create_and_get() {
        let (storage, profile, whoami, project) = setup();
        let author = whoami.urn();
        let issues = Issues::new(whoami, profile.paths(), &storage).unwrap();
        let issue_id = issues
            .create(&project.urn(), "My first issue", "Blah blah blah.")
            .unwrap();
        let issue = issues.get(&project.urn(), &issue_id).unwrap().unwrap();

        assert_eq!(issue.title(), "My first issue");
        assert_eq!(issue.author(), &author);
        assert_eq!(issue.description(), "Blah blah blah.");
        assert_eq!(issue.comments().len(), 0);
        assert_eq!(issue.state(), State::Open);
    }

    #[test]
    fn test_issue_create_and_change_state() {
        let (storage, profile, whoami, project) = setup();
        let issues = Issues::new(whoami, profile.paths(), &storage).unwrap();
        let issue_id = issues
            .create(&project.urn(), "My first issue", "Blah blah blah.")
            .unwrap();

        issues.close(&project.urn(), &issue_id).unwrap();

        let issue = issues.get(&project.urn(), &issue_id).unwrap().unwrap();
        assert_eq!(issue.state(), State::Closed);
    }

    #[test]
    fn test_issue_react() {
        let (storage, profile, whoami, project) = setup();
        let issues = Issues::new(whoami, profile.paths(), &storage).unwrap();
        let project = project.urn();
        let issue_id = issues
            .create(&project, "My first issue", "Blah blah blah.")
            .unwrap();

        let reaction = Reaction::new('🥳').unwrap();
        issues.react(&project, &issue_id, 0, reaction).unwrap();

        let issue = issues.get(&project, &issue_id).unwrap().unwrap();
        let count = issue.reactions()[&reaction];

        // TODO: Test multiple reactions from same author and different authors

        assert_eq!(count, 1);
    }

    #[test]
    fn test_issue_label() {
        let (storage, profile, whoami, project) = setup();
        let issues = Issues::new(whoami, profile.paths(), &storage).unwrap();
        let project = project.urn();
        let issue_id = issues
            .create(&project, "My first issue", "Blah blah blah.")
            .unwrap();

        let bug_label = Label::new("bug").unwrap();
        let wontfix_label = Label::new("wontfix").unwrap();

        issues
            .label(&project, &issue_id, &[bug_label.clone()])
            .unwrap();
        issues
            .label(&project, &issue_id, &[wontfix_label.clone()])
            .unwrap();

        let issue = issues.get(&project, &issue_id).unwrap().unwrap();
        let labels = issue.labels();

        assert!(labels.contains(&bug_label));
        assert!(labels.contains(&wontfix_label));
    }

    #[test]
    fn test_issue_comment() {
        let (storage, profile, whoami, project) = setup();
        let author = whoami.urn();
        let issues = Issues::new(whoami, profile.paths(), &storage).unwrap();
        let issue_id = issues
            .create(&project.urn(), "My first issue", "Blah blah blah.")
            .unwrap();

        issues
            .comment(&project.urn(), &issue_id, "Ho ho ho.")
            .unwrap();

        issues
            .comment(&project.urn(), &issue_id, "Ha ha ha.")
            .unwrap();

        let issue = issues.get(&project.urn(), &issue_id).unwrap().unwrap();
        let c1 = &issue.comments()[0];
        let c2 = &issue.comments()[1];

        assert_eq!(&c1.body, "Ho ho ho.");
        assert_eq!(&c1.author, &author);
        assert_eq!(&c2.body, "Ha ha ha.");
        assert_eq!(&c2.author, &author);
    }
}