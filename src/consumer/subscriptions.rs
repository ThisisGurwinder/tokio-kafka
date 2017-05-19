use std::hash::Hash;
use std::iter::FromIterator;
use std::collections::HashSet;

pub struct Subscriptions {
    /// the list of topics the user has requested
    subscription: HashSet<String>,

    /// the list of topics the group has subscribed to
    /// (set only for the leader on join group completion)
    group_subscription: HashSet<String>,
}

impl Subscriptions {
    pub fn new() -> Self {
        Subscriptions {
            subscription: HashSet::new(),
            group_subscription: HashSet::new(),
        }
    }

    pub fn with_topics<I: Iterator<Item = S>, S: AsRef<str> + Hash + Eq>(topic_names: I) -> Self {
        let topic_names: Vec<String> = topic_names.map(|s| s.as_ref().to_owned()).collect();

        Subscriptions {
            subscription: HashSet::from_iter(topic_names.iter().cloned()),
            group_subscription: HashSet::from_iter(topic_names.iter().cloned()),
        }
    }

    pub fn subscribe<I: Iterator<Item = S>, S: AsRef<str> + Hash + Eq>(&mut self, topic_names: I) {
        let topic_names: Vec<String> = topic_names.map(|s| s.as_ref().to_owned()).collect();
        self.subscription = HashSet::from_iter(topic_names.iter().cloned());
        self.group_subscription = &self.group_subscription | &self.subscription;
    }
}