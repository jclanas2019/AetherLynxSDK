use std::collections::HashMap;

pub struct IPAPolicy {
    pub identities: HashMap<String, Vec<String>>,
    pub resources: HashMap<String, Vec<String>>,
}

impl IPAPolicy {
    pub fn new() -> IPAPolicy {
        IPAPolicy {
            identities: HashMap::new(),
            resources: HashMap::new(),
        }
    }

    pub fn add_role(&mut self, username: &str, role: &str) {
        self.identities
            .entry(username.to_string())
            .or_insert_with(Vec::new)
            .push(role.to_string());
    }

    pub fn remove_role(&mut self, username: &str, role: &str) {
        if let Some(roles) = self.identities.get_mut(username) {
            roles.retain(|r| r != role);
        }
    }

    pub fn get_roles(&self, username: &str) -> Option<&Vec<String>> {
        self.identities.get(username)
    }

    pub fn add_resource(&mut self, resource: &str, allowed_role: &str) {
        self.resources
            .entry(resource.to_string())
            .or_insert_with(Vec::new)
            .push(allowed_role.to_string());
    }

    pub fn remove_resource(&mut self, resource: &str) {
        self.resources.remove(resource);
    }

    pub fn has_access(&self, username: &str, resource: &str) -> bool {
        if let Some(user_roles) = self.identities.get(username) {
            if let Some(allowed_roles) = self.resources.get(resource) {
                return user_roles.iter().any(|role| allowed_roles.contains(role));
            }
        }
        false
    }

    pub fn list_accessible_resources(&self, username: &str) -> Vec<String> {
        let mut accessible = Vec::new();
        if let Some(user_roles) = self.identities.get(username) {
            for (resource, allowed_roles) in &self.resources {
                if user_roles.iter().any(|role| allowed_roles.contains(role)) {
                    accessible.push(resource.clone());
                }
            }
        }
        accessible
    }
}