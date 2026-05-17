//! Profile validation. Plain functions + types so the form component has a
//! real trait/type surface behind its `view!`.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileDraft {
    pub name: String,
    pub email: String,
    pub age: String,
}

// `PartialEq` is required by `leptos::create_memo` (Memo<T>: T: PartialEq).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValidationReport {
    pub errors: Vec<String>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    fn push(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }
}

fn looks_like_email(s: &str) -> bool {
    let at = match s.find('@') {
        Some(i) => i,
        None => return false,
    };
    let (local, rest) = s.split_at(at);
    let domain = &rest[1..];
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

pub fn validate_profile(draft: &ProfileDraft) -> ValidationReport {
    let mut report = ValidationReport::default();

    let name = draft.name.trim();
    if name.is_empty() {
        report.push("name is required");
    } else if name.len() < 2 {
        report.push("name is too short");
    } else if name.len() > 64 {
        report.push("name is too long");
    }

    if draft.email.trim().is_empty() {
        report.push("email is required");
    } else if !looks_like_email(draft.email.trim()) {
        report.push("email looks invalid");
    }

    match draft.age.trim().parse::<i32>() {
        Ok(age) if age < 13 => report.push("must be 13 or older"),
        Ok(age) if age > 130 => report.push("age looks invalid"),
        Ok(_) => {}
        Err(_) if draft.age.trim().is_empty() => report.push("age is required"),
        Err(_) => report.push("age must be a number"),
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_profile_has_no_errors() {
        let d = ProfileDraft {
            name: "Ada".into(),
            email: "ada@example.com".into(),
            age: "30".into(),
        };
        assert!(validate_profile(&d).is_ok());
    }

    #[test]
    fn rejects_bad_email_and_age() {
        let d = ProfileDraft {
            name: "Ada".into(),
            email: "nope".into(),
            age: "abc".into(),
        };
        let r = validate_profile(&d);
        assert_eq!(r.errors.len(), 2);
    }
}
