//! Recreate a zoned carrier's instant from its explicit offset, then restore
//! its stored region. The postimage check also verifies TZD/offset consistency.
pub(super) fn zoned_bind(bind: &str) -> String {
    // SYYYY (including sign/blank) plus timestamp FF9 is exactly 30 ASCII
    // characters, followed by space, offset (6), space, region and optional TZD.
    // Keep ONE placeholder occurrence: OCI positional binds count occurrences.
    // A scalar subquery names that value so all conversions use the same bind.
    // OCI string binds may use the national UTF-16 character set. Oracle's
    // AT TIME ZONE expression requires the region in the database character
    // set; an explicit VARCHAR2 cast avoids ORA-01882 for valid NCHAR regions.
    format!("(SELECT CASE WHEN v IS NULL THEN CAST(NULL AS TIMESTAMP WITH TIME ZONE) ELSE (TO_TIMESTAMP_TZ(SUBSTR(v,1,37),'SYYYY-MM-DD\"T\"HH24:MI:SS.FF9 TZH:TZM','NLS_DATE_LANGUAGE=English') AT TIME ZONE CAST(REGEXP_SUBSTR(SUBSTR(v,39),'[^ ]+') AS VARCHAR2(128 BYTE))) END FROM (SELECT {bind} AS v FROM dual))")
}
