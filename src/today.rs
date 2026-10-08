use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::America::Sao_Paulo;

/// Dia civil em São Paulo de um instante.
pub fn local_day(at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&Sao_Paulo).date_naive()
}

/// 00:00 de `day` em São Paulo, em UTC. Se a meia-noite não existir (mudança de horário
/// histórica), usa o primeiro instante válido do dia.
pub fn day_start_utc(day: NaiveDate) -> DateTime<Utc> {
    (0..24)
        .find_map(|h| Sao_Paulo.from_local_datetime(&day.and_hms_opt(h, 0, 0).expect("hora válida")).earliest())
        .expect("todo dia tem um instante válido")
        .with_timezone(&Utc)
}

pub fn ago(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}min", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

/// "HH:MM" em São Paulo.
pub fn local_hm(at: DateTime<Utc>) -> String {
    at.with_timezone(&Sao_Paulo).format("%H:%M").to_string()
}

/// Única definição de "contagem válida": sucesso do dia de hoje (SP) para exatamente o conjunto
/// de repos atual e, quando `viewer` é conhecido, para essa mesma conta. Usada pelo `status` e
/// pelo TUI, que passam `None`: o estado não guarda outro viewer além do da própria contagem, e
/// o daemon já descarta `last_success` assim que o `gh` responde com outra conta.
pub fn valid_success<'a>(
    rt: &'a crate::state::ReviewsToday,
    now: DateTime<Utc>,
    repos: &[String],
    viewer: Option<&str>,
) -> Option<&'a crate::state::ReviewsTodaySuccess> {
    let s = rt.last_success.as_ref()?;
    let mut have = s.repos.clone();
    let mut want = repos.to_vec();
    have.sort();
    want.sort();
    (s.day == local_day(now) && have == want && viewer.is_none_or(|v| v == s.viewer)).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};

    #[test]
    fn day_in_sao_paulo() {
        // 02:30 UTC = 23:30 do dia anterior em SP (UTC-3)
        let at = Utc.with_ymd_and_hms(2026, 10, 8, 2, 30, 0).unwrap();
        assert_eq!(local_day(at), NaiveDate::from_ymd_opt(2026, 10, 7).unwrap());
        let start = day_start_utc(NaiveDate::from_ymd_opt(2026, 10, 7).unwrap());
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 10, 7, 3, 0, 0).unwrap());
    }

    #[test]
    fn ago_formats() {
        assert_eq!(ago(chrono::Duration::seconds(12)), "12s");
        assert_eq!(ago(chrono::Duration::seconds(190)), "3min");
        assert_eq!(ago(chrono::Duration::seconds(7300)), "2h");
        assert_eq!(ago(chrono::Duration::seconds(-5)), "0s");
    }

    #[test]
    fn valid_success_needs_today_and_same_repos() {
        use crate::state::{ReviewsToday, ReviewsTodaySuccess};
        let now = Utc.with_ymd_and_hms(2026, 10, 7, 15, 0, 0).unwrap();
        let mut rt = ReviewsToday::default();
        assert!(valid_success(&rt, now, &[], None).is_none());
        rt.last_success = Some(ReviewsTodaySuccess {
            day: local_day(now),
            count: 2,
            per_repo: Default::default(),
            as_of: now,
            viewer: "c".into(),
            repos: vec!["a/b".into(), "c/d".into()],
        });
        let same = vec!["c/d".to_string(), "a/b".to_string()];
        assert!(valid_success(&rt, now, &same, None).is_some());
        assert!(valid_success(&rt, now, &["a/b".to_string()], None).is_none());
        assert!(valid_success(&rt, now + chrono::Duration::days(1), &same, None).is_none());
        // viewer conhecido: só vale a contagem dele
        assert!(valid_success(&rt, now, &same, Some("c")).is_some());
        assert!(valid_success(&rt, now, &same, Some("outra")).is_none());
    }
}
