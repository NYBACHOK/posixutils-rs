use chrono::{DateTime, Utc};
use clap::Parser;
use gettextrs::{bind_textdomain_codeset, setlocale, textdomain, LocaleCategory};
use libc::{getlogin, getpwnam, getpwuid, passwd, uid_t};
use plib::PROJECT_NAME;
use timespec::Timespec;

use std::{
    env,
    ffi::{CStr, CString},
    fs::File,
    io::{BufRead, Read, Seek, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process,
    str::FromStr,
};

/*
    TODO:

    1. file with at.allow and at.deny
    first check is allowed, then is denied. If not found in any -> allow
    MAX chars for name in linux is 32

    2.

*/

// TODO: Mac variant?
const JOB_FILE_NUMBER: &str = "/var/spool/atjobs/.SEQ";
const SPOOL_DIRECTORY: &str = "/home/ghuba/";

const DAEMON_UID: u32 = 457;

/// at - execute commands at a later time
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "at - execute commands at a later time",
    long_about = "The 'at' command schedules commands to be executed later.\n\
                  Usage:\n\
                  at [-m] [-f file] [-q queuename] -t time_arg\n\
                  at [-m] [-f file] [-q queuename] timespec...\n\
                  at -r at_job_id...\n\
                  at -l -q queuename\n\
                  at -l [at_job_id...]"
)]
struct Args {
    /// Submit the job to be run at the date and time specified.
    #[arg(value_name = "TIMESPEC")]
    timespec: Option<String>,

    /// Change the environment to what would be expected if the user actually logged in again (letter `l`).
    #[arg(short = 'l', long)]
    login: bool,

    /// Specifies the pathname of a file to be used as the source of the at-job, instead of standard input.
    #[arg(short = 'f', long, value_name = "FILE")]
    file: Option<PathBuf>,

    /// Send mail to the invoking user after the at-job has run.
    #[arg(short = 'm', long)]
    mail: bool,

    /// Specify in which queue to schedule a job for submission.
    #[arg(short = 'q', long, value_name = "QUEUENAME", default_value = "a")]
    queue: String,

    /// Remove the jobs with the specified at_job_id operands that were previously scheduled by the at utility.
    #[arg(short = 'r', long)]
    remove: bool,

    /// Submit the job to be run at the time specified by the time option-argument.
    #[arg(short = 't', long, value_name = "TIME_ARG")]
    time: Option<String>,

    /// Group ID or group name.
    #[arg(value_name = "GROUP", required = false)]
    group: Option<String>,

    /// Job IDs for reporting jobs scheduled for the invoking user.
    #[arg(value_name = "AT_JOB_ID", required = false)]
    at_job_ids: Vec<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Args {
        timespec,
        login,
        file,
        mail,
        queue,
        remove,
        time,
        group,
        at_job_ids,
    } = Args::try_parse().unwrap_or_else(|err| {
        eprintln!("{}", err);
        std::process::exit(1);
    });

    setlocale(LocaleCategory::LcAll, "");
    textdomain(PROJECT_NAME)?;
    bind_textdomain_codeset(PROJECT_NAME, "UTF-8")?;

    // if args.mail {
    //     let real_uid = unsafe { libc::getuid() };
    //     let mut mailname = get_login_name();

    //     if mailname
    //         .as_ref()
    //         .and_then(|name| user_info_by_name(name))
    //         .is_none()
    //     {
    //         if let Some(pass_entry) = user_info_by_uid(real_uid) {
    //             mailname = unsafe {
    //                 // Safely convert pw_name using CString, avoiding memory leaks.
    //                 let cstr = CString::from_raw(pass_entry.pw_name as *mut i8);
    //                 cstr.to_str().ok().map(|s| s.to_string())
    //             };
    //         }
    //     }

    //     match mailname {
    //         Some(name) => println!("Mailname: {}", name),
    //         None => println!("Failed to retrieve mailname."),
    //     }
    // }

    let time = match (time, timespec) {
        (None, None) => print_err_and_exit(1, "You need `timespec` arg or `-t` flag"),
        (None, Some(timespec)) => Timespec::from_str(&timespec)?
            .to_date_time()
            .ok_or("Failed to parse `timespec` did you set too big date?")?,
        (Some(time), None) => time::parse_time_posix(&time)?,
        (Some(_), Some(_)) => print_err_and_exit(
            1,
            "You can't specify time twice. Use only `timespec` arg or `-t` flag",
        ),
    };

    let cmd = match file {
        Some(path) => {
            let path = match path.is_absolute() {
                true => path,
                false => std::env::current_dir().ok().unwrap_or_default().join(path),
            };

            let mut file = File::open(path)
                .map_err(|e| format!("Failed to open command file. Reason: {e}"))?;

            let mut buf = String::new();

            file.read_to_string(&mut buf)
                .map_err(|e| format!("Failed to read command file. Reason: {e}"))?;

            buf
        }
        None => {
            let stdout = std::io::stdout();
            let mut stdout_lock = stdout.lock();

            // TODO: Correct formating
            writeln!(&mut stdout_lock, "{}", time.to_rfc2822())?;
            write!(&mut stdout_lock, "at> ")?;
            stdout_lock.flush()?;

            let stdin = std::io::stdin();
            let mut stdin_lock = stdin.lock();

            let mut result = Vec::new();
            let mut buf = String::new();

            while stdin_lock.read_line(&mut buf)? != 0 {
                write!(&mut stdout_lock, "at> ")?;
                stdout_lock.flush()?;

                result.push(buf.to_owned());
            }

            write!(&mut stdout_lock, "<EOT>\n")?;
            stdout_lock.flush()?;

            result.join("\n")
        }
    };

    let _ = at(&queue, &time, cmd).inspect_err(|err| print_err_and_exit(1, err));

    Ok(())
}

fn print_err_and_exit(exit_code: i32, err: impl std::fmt::Display) -> ! {
    eprintln!("{}", err);
    process::exit(exit_code)
}

fn at(
    queue: &str,
    execution_time: &DateTime<Utc>,
    cmd: impl Into<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let jobno = next_job_id()?;
    let job_filename = job_file_name(jobno, queue, execution_time)
        .ok_or("Failed to generate file name for job")?;

    let user = User::new().ok_or("Failed to get current user")?;

    let job = Job::new(&user, std::env::current_dir()?, std::env::vars(), cmd).into_script();

    let mut file_opt = std::fs::OpenOptions::new();
    file_opt.read(true).write(true).create_new(true);

    let file_path = PathBuf::from(format!("{SPOOL_DIRECTORY}/{job_filename}"));

    let mut file = file_opt
        .open(&file_path)
        .map_err(|e| format!("Failed to create file with job. Reason: {e}"))?;

    file.write_all(job.as_bytes())?;

    std::os::unix::fs::chown(file_path, Some(user.uid), Some(user.gid))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o700))?;

    Ok(())
}

/// Structure to represent future job or script to be saved in [`SPOOL_DIRECTORY`]
pub struct Job {
    shell: String,
    user_uid: u32,
    user_gid: u32,
    user_name: String,
    env: std::env::Vars,
    call_place: PathBuf,
    cmd: String,
}

impl Job {
    pub fn new(
        User {
            shell,
            uid,
            gid,
            name,
        }: &User,
        call_place: PathBuf,
        env: std::env::Vars,
        cmd: impl Into<String>,
    ) -> Self {
        Self {
            shell: shell.to_owned(),
            user_uid: *uid,
            user_gid: *gid,
            user_name: name.to_owned(),
            env,
            call_place,
            cmd: cmd.into(),
        }
    }

    pub fn into_script(self) -> String {
        let Self {
            shell,
            user_uid,
            user_gid,
            user_name,
            env,
            call_place,
            cmd,
        } = self;

        let env = env
            .into_iter()
            .map(|(key, value)| format!("{}={}; export {}", key, value, key))
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            "#!{shell}\n# atrun uid={user_uid} gid={user_gid}\n# mail {user_name} 0\numask 22\n{env}\ncd {} || {{\n\techo 'Execution directory inaccessible' >&2\n\texit 1 \n}}\n{cmd}",
            call_place.to_string_lossy()
        )
    }
}

/// Return name for job number
///
/// None if DateTime < [DateTime::UNIX_EPOCH]
fn job_file_name(next_job: u32, queue: &str, time: &DateTime<Utc>) -> Option<String> {
    let duration = time.signed_duration_since(DateTime::UNIX_EPOCH);
    let duration_seconds = u32::try_from(duration.num_seconds()).ok()? / 60;

    let result = format!("{queue}{next_job:05x}{duration_seconds:08x}");

    Some(result)
}

#[derive(Debug)]
pub enum NextJobError {
    Io(std::io::Error),
    FromStr(std::num::ParseIntError),
}

impl std::fmt::Display for NextJobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Error reading id of next job. Reason: ")?;
        match self {
            NextJobError::Io(err) => writeln!(f, "{err}"),
            NextJobError::FromStr(err) => writeln!(f, "invalid number - {err}"),
        }
    }
}

impl std::error::Error for NextJobError {}

fn next_job_id() -> Result<u32, NextJobError> {
    let mut file_opt = std::fs::OpenOptions::new();
    file_opt.read(true).write(true);

    let mut buf = String::new();

    let (next_job_id, mut file) = match file_opt.open(JOB_FILE_NUMBER) {
        Ok(mut file) => {
            file.read_to_string(&mut buf)
                .map_err(|e| NextJobError::Io(e))?;
            file.rewind().map_err(|e| NextJobError::Io(e))?;

            (
                u32::from_str_radix(buf.trim_end_matches("\n"), 16)
                    .map_err(|e| NextJobError::FromStr(e))?,
                file,
            )
        }
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => (
                0,
                std::fs::File::create_new(JOB_FILE_NUMBER).map_err(|e| NextJobError::Io(e))?,
            ),
            _ => Err(NextJobError::Io(err))?,
        },
    };

    // Limit range of jobs to 2^20 jobs
    let next_job_id = (1 + next_job_id) % 0xfffff;

    file.write_all(format!("{next_job_id:05x}").as_bytes())
        .map_err(|e| NextJobError::Io(e))?;

    Ok(next_job_id)
}

fn login_name() -> Option<String> {
    // Try to get the login name using getlogin
    unsafe {
        let login_ptr = getlogin();
        if !login_ptr.is_null() {
            if let Ok(c_str) = CStr::from_ptr(login_ptr).to_str() {
                return Some(c_str.to_string());
            }
        }
    }

    // Fall back to checking the LOGNAME environment variable
    env::var("LOGNAME").ok()
}

pub struct User {
    pub name: String,
    pub shell: String,
    pub uid: u32,
    pub gid: u32,
}

impl User {
    pub fn new() -> Option<Self> {
        const DEFAULT_SHELL: &str = "/bin/sh";

        let login_name = login_name()?;

        let passwd {
            pw_uid,
            pw_gid,
            pw_shell,
            ..
        } = user_info_by_name(&login_name)?;

        let pw_shell = match pw_shell.is_null() {
            true => std::env::var("SHELL")
                .ok()
                .unwrap_or(DEFAULT_SHELL.to_owned()),
            false => unsafe {
                CStr::from_ptr(pw_shell)
                    .to_str()
                    .ok()
                    .unwrap_or(DEFAULT_SHELL)
                    .to_owned()
            },
        };

        Some(Self {
            shell: pw_shell,
            uid: pw_uid,
            gid: pw_gid,
            name: login_name,
        })
    }
}

fn user_info_by_name(name: &str) -> Option<passwd> {
    let c_name = CString::new(name).unwrap();
    let pw_ptr = unsafe { getpwnam(c_name.as_ptr()) };
    if pw_ptr.is_null() {
        None
    } else {
        Some(unsafe { *pw_ptr })
    }
}

mod time {
    use chrono::{offset::LocalResult, DateTime, Datelike, TimeZone, Utc};

    // Copy from `touch`
    pub fn parse_time_posix(time: &str) -> Result<DateTime<Utc>, Box<dyn std::error::Error>> {
        let mut time = String::from(time);
        let mut seconds = String::from("0");

        // split into YYYYMMDDhhmm and [.SS] components
        let mut tmp_time = String::new();
        match time.split_once('.') {
            Some((t, secs)) => {
                tmp_time = t.to_string();
                seconds = secs.to_string();
            }
            None => {}
        }
        if !tmp_time.is_empty() {
            time = tmp_time;
        }

        // extract date and time elements, with length implying format
        let tmp_year;
        let (year_str, month_str, day_str, hour_str, minute_str) = match time.len() {
            // format: MMDDhhmm[.SS]
            8 => {
                tmp_year = Utc::now().year().to_string();
                (
                    tmp_year.as_str(),
                    &time[0..2],
                    &time[2..4],
                    &time[4..6],
                    &time[6..8],
                )
            }

            // format: YYMMDDhhmm[.SS]
            10 => {
                let mut yearling = time[0..2].parse::<u32>()?;
                if yearling <= 68 {
                    yearling += 2000;
                } else {
                    yearling += 1900;
                }
                tmp_year = yearling.to_string();
                (
                    tmp_year.as_str(),
                    &time[2..4],
                    &time[4..6],
                    &time[6..8],
                    &time[8..10],
                )
            }

            // format: YYYYMMDDhhmm[.SS]
            12 => (
                &time[0..4],
                &time[4..6],
                &time[6..8],
                &time[8..10],
                &time[10..12],
            ),
            _ => {
                return Err("Invalid time format".into());
            }
        };

        // convert strings to integers
        let year = year_str.parse::<i32>()?;
        let month = month_str.parse::<u32>()?;
        let day = day_str.parse::<u32>()?;
        let hour = hour_str.parse::<u32>()?;
        let minute = minute_str.parse::<u32>()?;
        let secs = seconds.parse::<u32>()?;

        // convert to DateTime and validate input
        let res = Utc.with_ymd_and_hms(year, month, day, hour, minute, secs);
        if res == LocalResult::None {
            return Err("Invalid time".into());
        }

        // return parsed date
        let dt = res.unwrap();
        Ok(dt)
    }
}

mod timespec {
    use std::{num::NonZero, str::FromStr};

    use chrono::{DateTime, Datelike, Days, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Utc};

    use crate::tokens::{
        AmPm, DayNumber, DayOfWeek, Hr24Clock, Hr24ClockHour, Minute, Month, TimezoneName,
        TokenParsingError, WallClock, WallClockHour, YearNumber,
    };

    #[derive(Debug, PartialEq)]
    pub enum TimespecParsingError {
        IncPeriodPatternNotFound(String),
        IncrementParsing {
            err: std::num::ParseIntError,
            input: String,
        },
        IncrementPatternNotFound(String),
        DateTokenParsing(TokenParsingError),
        DatePatternNotFound(String),
        TimeTokenParsing(TokenParsingError),
        TimePatternNotFound(String),
        NowspecParsing(String),
        NowspecPatternNotFound(String),
        TimespecPatternNotFound(String),
    }

    impl std::fmt::Display for TimespecParsingError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            writeln!(f, "Failed to parse token in str")
        }
    }

    impl std::error::Error for TimespecParsingError {}

    #[derive(Debug, PartialEq)]
    pub enum IncPeriod {
        Minute,
        Hour,
        Day,
        Week,
        Month,
        Year,
    }

    impl FromStr for IncPeriod {
        type Err = TimespecParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let result = match s {
                "minute" | "minutes" => Self::Minute,
                "hour" | "hours" => Self::Hour,
                "day" | "days" => Self::Day,
                "week" | "weeks" => Self::Week,
                "month" | "months" => Self::Month,
                "year" | "years" => Self::Year,
                _ => Err(TimespecParsingError::IncPeriodPatternNotFound(s.to_owned()))?,
            };

            Ok(result)
        }
    }

    #[derive(Debug, PartialEq)]
    pub enum Increment {
        Next(IncPeriod),
        Plus { number: u16, period: IncPeriod },
    }

    impl Increment {
        pub fn to_duration(&self) -> std::time::Duration {
            fn increment_date(number: Option<u16>, period: &IncPeriod) -> std::time::Duration {
                let number = u64::from(number.unwrap_or(1));

                match period {
                    IncPeriod::Minute => std::time::Duration::from_secs(60 * number),
                    IncPeriod::Hour => std::time::Duration::from_secs((60 * 60) * number),
                    IncPeriod::Day => std::time::Duration::from_secs((60 * 60 * 24) * number),
                    IncPeriod::Week => std::time::Duration::from_secs((60 * 60 * 24 * 7) * number),
                    IncPeriod::Month => std::time::Duration::from_secs(2628000 * number),
                    IncPeriod::Year => std::time::Duration::from_secs(31557600 * number),
                }
            }

            match self {
                Increment::Next(period) => increment_date(None, period),
                Increment::Plus { number, period } => increment_date(Some(*number), period),
            }
        }
    }

    impl FromStr for Increment {
        type Err = TimespecParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let result = match s.starts_with("+") {
                true => {
                    let number: u16 = s
                        .chars()
                        .skip(1)
                        .take_while(|this| this.is_numeric())
                        .collect::<String>()
                        .parse()
                        .map_err(|err| TimespecParsingError::IncrementParsing {
                            err,
                            input: s.to_owned(),
                        })?;

                    let period = s
                        .chars()
                        .skip(1)
                        .skip_while(|this| this.is_numeric())
                        .collect::<String>()
                        .parse::<IncPeriod>()?;

                    Self::Plus { number, period }
                }
                false => match s.starts_with("next") {
                    true => Self::Next(IncPeriod::from_str(&s.replace("next", ""))?),
                    false => Err(TimespecParsingError::IncrementPatternNotFound(s.to_owned()))?,
                },
            };

            Ok(result)
        }
    }

    #[derive(Debug, PartialEq)]
    pub enum Date {
        MontDay {
            month_name: Month,
            day_number: DayNumber,
        },
        MontDayYear {
            month_name: Month,
            day_number: DayNumber,
            year_number: YearNumber,
        },
        DayOfWeek(DayOfWeek),
        Today,
        Tomorrow,
    }

    impl Date {
        pub fn to_naive_date(&self) -> Option<chrono::NaiveDate> {
            match self {
                Date::MontDay {
                    month_name,
                    day_number,
                } => NaiveDate::from_ymd_opt(
                    Utc::now().year(),
                    u32::from(month_name.0) + 1,
                    day_number.0.get().into(),
                ),
                Date::MontDayYear {
                    month_name,
                    day_number,
                    year_number,
                } => NaiveDate::from_ymd_opt(
                    year_number.0.into(),
                    u32::from(month_name.0) + 1,
                    day_number.0.get().into(),
                ),
                Date::DayOfWeek(day_of_week) => {
                    let now = Utc::now();

                    // Check if date should be in current week or next
                    let n = match day_of_week.0.num_days_from_sunday()
                        > now.weekday().number_from_sunday()
                    {
                        true => 1,
                        false => 2,
                    };

                    NaiveDate::from_weekday_of_month_opt(now.year(), now.month(), day_of_week.0, n)
                }
                Date::Today => Some(Utc::now().date_naive()),
                Date::Tomorrow => Utc::now()
                    .checked_add_days(Days::new(1))
                    .map(|this| this.date_naive()),
            }
        }
    }

    impl FromStr for Date {
        type Err = TimespecParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let result = match s {
                "today" => Self::Today,
                "tomorrow" => Self::Tomorrow,
                _ => match s.contains(",") {
                    true => {
                        let parts = s.split(',').collect::<Vec<_>>();

                        if parts.len() != 2 {
                            Err(TimespecParsingError::DatePatternNotFound(s.to_owned()))?
                        }

                        let (month_name, day_number) = parse_month_and_day(&parts[0])?;
                        let year_number = YearNumber::from_str(&parts[1])
                            .map_err(|e| TimespecParsingError::DateTokenParsing(e))?;

                        Self::MontDayYear {
                            month_name,
                            day_number,
                            year_number,
                        }
                    }
                    false => match DayOfWeek::from_str(s) {
                        Ok(day) => Self::DayOfWeek(day),
                        Err(_) => {
                            let (month_name, day_number) = parse_month_and_day(s)?;

                            Self::MontDay {
                                month_name,
                                day_number,
                            }
                        }
                    },
                },
            };

            fn parse_month_and_day(s: &str) -> Result<(Month, DayNumber), TimespecParsingError> {
                let month = s
                    .chars()
                    .take_while(|this| !this.is_numeric())
                    .collect::<String>()
                    .parse::<Month>()
                    .map_err(|e| TimespecParsingError::DateTokenParsing(e))?;

                let day = s
                    .chars()
                    .skip_while(|this| !this.is_numeric())
                    .collect::<String>()
                    .parse::<DayNumber>()
                    .map_err(|e| TimespecParsingError::DateTokenParsing(e))?;

                Ok((month, day))
            }

            Ok(result)
        }
    }

    #[derive(Debug, PartialEq)]
    pub enum Time {
        Midnight,
        Noon,
        Hr24clockHour(Hr24Clock),
        Hr24clockHourTimezone {
            hour: Hr24Clock,
            timezone: TimezoneName,
        },
        Hr24clockHourMinute {
            hour: Hr24ClockHour,
            minute: Minute,
        },
        Hr24clockHourMinuteTimezone {
            hour: Hr24ClockHour,
            minute: Minute,
            timezone: TimezoneName,
        },
        WallclockHour {
            clock: WallClock,
            am: AmPm,
        },
        WallclockHourTimezone {
            clock: WallClock,
            am: AmPm,
            timezone: TimezoneName,
        },
        WallclockHourMinute {
            clock: WallClockHour,
            minute: Minute,
            am: AmPm,
        },
        WallclockHourMinuteTimezone {
            clock: WallClockHour,
            minute: Minute,
            am: AmPm,
            timezone: TimezoneName,
        },
    }

    impl Time {
        pub fn to_naive_time(&self) -> Option<chrono::NaiveTime> {
            match self {
                Time::Midnight => NaiveTime::from_hms_opt(0, 0, 0),
                Time::Noon => NaiveTime::from_hms_opt(12, 0, 0),
                Time::Hr24clockHour(hr24_clock) => {
                    let Hr24Clock([hour, minute]) = *hr24_clock;

                    NaiveTime::from_hms_opt(hour.into(), minute.into(), 0)
                }
                Time::Hr24clockHourTimezone { hour, timezone } => {
                    let Hr24Clock([hour, minute]) = *hour;

                    NaiveTime::from_hms_opt(hour.into(), minute.into(), 0)
                }
                Time::Hr24clockHourMinute { hour, minute } => {
                    let Hr24ClockHour(hour) = *hour;
                    let Minute(minute) = *minute;

                    NaiveTime::from_hms_opt(hour.into(), minute.into(), 0)
                }
                Time::Hr24clockHourMinuteTimezone {
                    hour,
                    minute,
                    timezone,
                } => {
                    let Hr24ClockHour(hour) = *hour;
                    let Minute(minute) = *minute;

                    NaiveTime::from_hms_opt(hour.into(), minute.into(), 0)
                }
                Time::WallclockHour { clock, am } => {
                    let WallClock { hour, minutes } = *clock;

                    chrono::NaiveTime::from_hms_opt(
                        u32::from(match am {
                            AmPm::Am => hour.get(),
                            AmPm::Pm => hour.get() + 12,
                        }),
                        u32::from(minutes),
                        0,
                    )
                }
                Time::WallclockHourTimezone {
                    clock,
                    am,
                    timezone,
                } => {
                    let WallClock { hour, minutes } = *clock;

                    chrono::NaiveTime::from_hms_opt(
                        u32::from(match am {
                            AmPm::Am => hour.get(),
                            AmPm::Pm => hour.get() + 12,
                        }),
                        u32::from(minutes),
                        0,
                    )
                }
                Time::WallclockHourMinute { clock, minute, am } => {
                    let WallClockHour(hour) = *clock;
                    let Minute(minutes) = *minute;

                    chrono::NaiveTime::from_hms_opt(
                        u32::from(match am {
                            AmPm::Am => hour.get(),
                            AmPm::Pm => hour.get() + 12,
                        }),
                        u32::from(minutes),
                        0,
                    )
                }
                Time::WallclockHourMinuteTimezone {
                    clock,
                    minute,
                    am,
                    timezone,
                } => {
                    let WallClockHour(hour) = *clock;
                    let Minute(minutes) = *minute;

                    chrono::NaiveTime::from_hms_opt(
                        u32::from(match am {
                            AmPm::Am => hour.get(),
                            AmPm::Pm => hour.get() + 12,
                        }),
                        u32::from(minutes),
                        0,
                    )
                }
            }
        }
    }

    impl FromStr for Time {
        type Err = TimespecParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            match s {
                "noon" => return Ok(Self::Noon),
                "midnight" => return Ok(Self::Midnight),
                _ => (),
            };

            if let Ok(hour) = Hr24Clock::from_str(s) {
                return Ok(Self::Hr24clockHour(hour));
            }

            if let Some((possible_hour, other)) = s.split_once(':') {
                if let Ok(clock) = WallClockHour::from_str(possible_hour) {
                    let minute = other
                        .chars()
                        .take_while(|this: &char| this.is_numeric())
                        .collect::<String>();
                    let minutes_len = minute.len();
                    let minute = Minute::from_str(&minute)
                        .map_err(|e| TimespecParsingError::TimeTokenParsing(e))?;

                    let other = other.chars().skip(minutes_len).collect::<String>();

                    if let Ok(am) = AmPm::from_str(&other) {
                        return Ok(Self::WallclockHourMinute { clock, minute, am });
                    }

                    let am = AmPm::from_str(&other[..2])
                        .map_err(|e| TimespecParsingError::TimeTokenParsing(e))?;
                    let timezone = TimezoneName::from_str(&other[2..])
                        .map_err(|e| TimespecParsingError::TimeTokenParsing(e))?;

                    return Ok(Self::WallclockHourMinuteTimezone {
                        clock,
                        minute,
                        am,
                        timezone,
                    });
                }

                if let Ok(hour) = Hr24ClockHour::from_str(possible_hour) {
                    let result = match Minute::from_str(other) {
                        Ok(minute) => Self::Hr24clockHourMinute { hour, minute },
                        Err(_) => {
                            let minute = other
                                .chars()
                                .take_while(|this| this.is_numeric())
                                .collect::<String>()
                                .parse::<Minute>()
                                .map_err(|e| TimespecParsingError::TimeTokenParsing(e))?;

                            let timezone = other
                                .chars()
                                .skip_while(|this| this.is_numeric())
                                .collect::<String>()
                                .parse::<TimezoneName>()
                                .map_err(|e| TimespecParsingError::TimeTokenParsing(e))?;

                            Self::Hr24clockHourMinuteTimezone {
                                hour,
                                minute,
                                timezone,
                            }
                        }
                    };

                    return Ok(result);
                }
            }

            let number = s
                .chars()
                .take_while(|this| this.is_numeric())
                .collect::<String>();
            let other = s
                .chars()
                .skip_while(|this| this.is_numeric())
                .collect::<String>();

            if let Ok(clock) = WallClock::from_str(&number) {
                if let Ok(am) = AmPm::from_str(&other[..2]) {
                    let timezone = TimezoneName::from_str(&other[2..]);
                    if let Ok(timezone) = timezone {
                        return Ok(Self::WallclockHourTimezone {
                            clock,
                            am,
                            timezone,
                        });
                    } else {
                        return Ok(Self::WallclockHour { clock, am });
                    }
                }
            }

            if let Ok(hour) = Hr24Clock::from_str(&number) {
                let timezone = TimezoneName::from_str(&other);

                if let Ok(timezone) = timezone {
                    return Ok(Self::Hr24clockHourTimezone { hour, timezone });
                }
            }

            Err(TimespecParsingError::TimePatternNotFound(s.to_owned()))
        }
    }

    #[derive(Debug, PartialEq)]
    pub enum Nowspec {
        Now,
        NowIncrement(Increment),
    }

    impl FromStr for Nowspec {
        type Err = TimespecParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            const NOW: &str = "now";

            let result = match s {
                NOW => Self::Now,
                _ if s.starts_with(NOW) => {
                    let (_, increment) = s
                        .split_once(NOW)
                        .ok_or(TimespecParsingError::NowspecParsing(s.to_owned()))?;

                    Self::NowIncrement(Increment::from_str(increment)?)
                }
                _ => Err(TimespecParsingError::NowspecPatternNotFound(s.to_owned()))?,
            };

            Ok(result)
        }
    }

    #[derive(Debug, PartialEq)]
    pub enum Timespec {
        Time(Time),
        TimeDate {
            time: Time,
            date: Date,
        },
        TimeDateIncrement {
            time: Time,
            date: Date,
            inrement: Increment,
        },
        Nowspec(Nowspec),
    }

    impl FromStr for Timespec {
        type Err = TimespecParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            if let Ok(time) = Time::from_str(s) {
                return Ok(Self::Time(time));
            }

            if let Ok(time) = Nowspec::from_str(s) {
                return Ok(Self::Nowspec(time));
            }

            let string_length = s.len();
            let mut time_index = 0;

            for slice_index in (0..string_length).rev() {
                let time = Time::from_str(&s[..slice_index]);
                if let Ok(_) = time {
                    time_index = slice_index;
                    break;
                }

                if slice_index == 0 {
                    Err(TimespecParsingError::TimespecPatternNotFound(s.to_owned()))?;
                }
            }

            let time = Time::from_str(&s[..time_index])?;

            let mut date_index = 0;
            for slice_index in (time_index..=string_length).rev() {
                let date = Date::from_str(&s[time_index..slice_index]);
                if let Ok(_) = date {
                    date_index = slice_index;
                    break;
                }

                if slice_index == time_index {
                    Err(TimespecParsingError::TimespecPatternNotFound(s.to_owned()))?;
                }
            }

            let date = Date::from_str(&s[time_index..date_index])?;

            if date_index != string_length {
                let inrement = Increment::from_str(&s[date_index..])?;

                return Ok(Self::TimeDateIncrement {
                    time,
                    date,
                    inrement,
                });
            }

            Ok(Self::TimeDate { time, date })
        }
    }

    impl Timespec {
        pub fn to_date_time(&self) -> Option<DateTime<Utc>> {
            let date_time = match self {
                Timespec::Time(time) => {
                    let time = time.to_naive_time()?;
                    let now = Utc::now();

                    match time < now.time() {
                        true => now
                            .checked_add_days(Days::new(1))?
                            .with_time(time)
                            .single()?
                            .to_utc(),
                        false => now.with_time(time).single()?.to_utc(),
                    }
                }
                Timespec::TimeDate { time, date } => {
                    let time = time.to_naive_time()?;
                    let date = date.to_naive_date()?;

                    // TODO: Unsure about this case
                    let date_time = NaiveDateTime::new(date, time).and_utc();
                    let date_time = match date_time < Utc::now() {
                        true => date_time.checked_add_months(chrono::Months::new(12))?,
                        false => date_time,
                    };

                    date_time
                }
                Timespec::TimeDateIncrement {
                    time,
                    date,
                    inrement,
                } => {
                    let time = time.to_naive_time()?;
                    let date = date.to_naive_date()?;

                    let date_time = NaiveDateTime::new(date, time)
                        .and_utc()
                        .checked_add_signed(
                            chrono::TimeDelta::from_std(inrement.to_duration()).ok()?,
                        )?;

                    // TODO: Unsure about this case
                    let date_time = match date_time < Utc::now() {
                        true => date_time.checked_add_months(chrono::Months::new(12))?,
                        false => date_time,
                    };

                    date_time
                }
                Timespec::Nowspec(nowspec) => match nowspec {
                    Nowspec::Now => Utc::now(),
                    Nowspec::NowIncrement(increment) => Utc::now().checked_add_signed(
                        chrono::TimeDelta::from_std(increment.to_duration()).ok()?,
                    )?,
                },
            };

            Some(date_time)
        }
    }

    #[cfg(test)]
    mod test {
        use std::num::NonZero;

        use super::*;

        // increment
        #[test]
        fn increment_period_simple() {
            let actual = Increment::from_str("+1day");

            assert_eq!(
                Ok(Increment::Plus {
                    number: 1,
                    period: IncPeriod::Day
                }),
                actual
            )
        }

        #[test]
        fn increment_period_two_numbers() {
            let actual = Increment::from_str("+12days");

            assert_eq!(
                Ok(Increment::Plus {
                    number: 12,
                    period: IncPeriod::Day
                }),
                actual
            )
        }

        #[test]
        fn increment_period_next() {
            let actual = Increment::from_str("nextday");

            assert_eq!(Ok(Increment::Next(IncPeriod::Day)), actual)
        }

        #[test]
        fn increment_period_no_number_after_sign() {
            let actual = Increment::from_str("+day");

            assert!(actual.is_err())
        }

        #[test]
        fn increment_period_empty() {
            let actual = Increment::from_str("");

            assert_eq!(
                Err(TimespecParsingError::IncrementPatternNotFound(String::new())),
                actual
            )
        }

        #[test]
        fn increment_period_next_no_perid_fails() {
            let actual = Increment::from_str("next");

            assert_eq!(
                Err(TimespecParsingError::IncPeriodPatternNotFound(String::new())),
                actual
            )
        }

        // nowspec

        #[test]
        fn nowspec_simple_now() {
            let actual = Nowspec::from_str("now");

            assert_eq!(Ok(Nowspec::Now), actual)
        }

        #[test]
        fn nowspec_incremented_number() {
            let actual = Nowspec::from_str("now+1day");

            assert_eq!(
                Ok(Nowspec::NowIncrement(Increment::Plus {
                    number: 1,
                    period: IncPeriod::Day
                })),
                actual
            );
        }

        #[test]
        fn nowspec_incremented_by_next() {
            let actual = Nowspec::from_str("nownextday");

            assert_eq!(
                Ok(Nowspec::NowIncrement(Increment::Next(IncPeriod::Day))),
                actual
            );
        }

        // time

        #[test]
        fn time_simple_24_hour_single_number() {
            let actual = Time::from_str("1");

            assert_eq!(Ok(Time::Hr24clockHour(Hr24Clock([1, 0]))), actual)
        }

        #[test]
        fn time_24_hour_full() {
            let actual = Time::from_str("1453");

            assert_eq!(Ok(Time::Hr24clockHour(Hr24Clock([14, 53]))), actual)
        }

        #[test]
        fn time_24_hour_noon() {
            let actual = Time::from_str("noon");

            assert_eq!(Ok(Time::Noon), actual)
        }

        #[test]
        fn time_24_hour_midnight() {
            let actual = Time::from_str("midnight");

            assert_eq!(Ok(Time::Midnight), actual)
        }

        #[test]
        fn time_24_hour_plus_timezone() {
            let actual = Time::from_str("1453UTC");

            assert_eq!(
                Ok(Time::Hr24clockHourTimezone {
                    hour: Hr24Clock([14, 53]),
                    timezone: TimezoneName("UTC".to_owned())
                }),
                actual
            )
        }

        #[test]
        fn time_24_hour_plus_timezone_2_digits() {
            let actual = Time::from_str("14UTC");

            assert_eq!(
                Ok(Time::Hr24clockHourTimezone {
                    hour: Hr24Clock([14, 00]),
                    timezone: TimezoneName("UTC".to_owned())
                }),
                actual
            )
        }

        #[test]
        fn time_hr24clock_hour_plus_minute() {
            let actual = Time::from_str("14:53");

            assert_eq!(
                Ok(Time::Hr24clockHourMinute {
                    hour: Hr24ClockHour(14),
                    minute: Minute(53)
                }),
                actual
            )
        }

        #[test]
        fn time_hr24clock_hour_plus_minute_timezone() {
            let actual = Time::from_str("14:53UTC");

            assert_eq!(
                Ok(Time::Hr24clockHourMinuteTimezone {
                    hour: Hr24ClockHour(14),
                    minute: Minute(53),
                    timezone: TimezoneName("UTC".to_owned())
                }),
                actual
            )
        }

        #[test]
        fn time_wallclock_hr_min_am() {
            let actual = Time::from_str("0500am");

            assert_eq!(
                Ok(Time::WallclockHour {
                    clock: WallClock {
                        hour: NonZero::new(5).expect("valid"),
                        minutes: 0
                    },
                    am: AmPm::Am
                }),
                actual
            )
        }

        #[test]
        fn time_wallclock_hr_min_plus_am_timezone() {
            let actual = Time::from_str("0500amUTC");

            assert_eq!(
                Ok(Time::WallclockHourTimezone {
                    clock: WallClock {
                        hour: NonZero::new(5).expect("valid"),
                        minutes: 0
                    },
                    am: AmPm::Am,
                    timezone: TimezoneName("UTC".to_owned())
                }),
                actual
            )
        }

        #[test]
        fn time_wallclock_hour_minute_am() {
            let actual = Time::from_str("05:53am");

            assert_eq!(
                Ok(Time::WallclockHourMinute {
                    clock: WallClockHour(NonZero::new(5).expect("valid")),
                    minute: Minute(53),
                    am: AmPm::Am,
                }),
                actual
            )
        }

        #[test]
        fn time_wallclock_hour_minute_am_timezone() {
            let actual = Time::from_str("05:53amUTC");

            assert_eq!(
                Ok(Time::WallclockHourMinuteTimezone {
                    clock: WallClockHour(NonZero::new(5).expect("valid")),
                    minute: Minute(53),
                    am: AmPm::Am,
                    timezone: TimezoneName("UTC".to_owned())
                }),
                actual
            )
        }

        // date

        #[test]
        fn date_month_name_day_number() {
            let actual = Date::from_str("NOV4");

            assert_eq!(
                Ok(Date::MontDay {
                    month_name: Month(10),
                    day_number: DayNumber(NonZero::new(4).expect("valid"))
                }),
                actual
            );
        }

        #[test]
        fn date_month_name_day_number_year_number() {
            let actual = Date::from_str("NOV4,2024");

            assert_eq!(
                Ok(Date::MontDayYear {
                    month_name: Month(10),
                    day_number: DayNumber(NonZero::new(4).expect("valid")),
                    year_number: YearNumber(2024)
                }),
                actual
            );
        }

        #[test]
        fn date_day_of_week() {
            let actual = Date::from_str("MON");

            assert_eq!(Ok(Date::DayOfWeek(DayOfWeek(chrono::Weekday::Mon))), actual);
        }

        #[test]
        fn date_today() {
            let actual = Date::from_str("today");

            assert_eq!(Ok(Date::Today), actual);
        }

        #[test]
        fn date_tomorrow() {
            let actual = Date::from_str("tomorrow");

            assert_eq!(Ok(Date::Tomorrow), actual);
        }

        // timespec

        #[test]
        fn timespec_time_24_hour_full() {
            let actual = Timespec::from_str("1453");

            assert_eq!(
                Ok(Timespec::Time(Time::Hr24clockHour(Hr24Clock([14, 53])))),
                actual
            )
        }

        #[test]
        fn timespec_time_24_hour_plus_timezone_2_digits() {
            let actual = Timespec::from_str("14UTC");

            assert_eq!(
                Ok(Timespec::Time(Time::Hr24clockHourTimezone {
                    hour: Hr24Clock([14, 00]),
                    timezone: TimezoneName("UTC".to_owned())
                })),
                actual
            )
        }

        #[test]
        fn timespec_time_hr24clock_hour_plus_minute() {
            let actual = Timespec::from_str("14:53");

            assert_eq!(
                Ok(Timespec::Time(Time::Hr24clockHourMinute {
                    hour: Hr24ClockHour(14),
                    minute: Minute(53)
                })),
                actual
            )
        }

        #[test]
        fn timespec_time_wallclock_hour_minute_am_timezone() {
            let actual = Timespec::from_str("05:53amUTC");

            assert_eq!(
                Ok(Timespec::Time(Time::WallclockHourMinuteTimezone {
                    clock: WallClockHour(NonZero::new(5).expect("valid")),
                    minute: Minute(53),
                    am: AmPm::Am,
                    timezone: TimezoneName("UTC".to_owned())
                })),
                actual
            )
        }

        #[test]
        fn timespec_nowspec() {
            let actual = Timespec::from_str("now");

            assert_eq!(Ok(Timespec::Nowspec(Nowspec::Now)), actual)
        }

        #[test]
        fn timespec_time_date() {
            let actual = Timespec::from_str("05:53amUTCtoday");

            assert_eq!(
                Ok(Timespec::TimeDate {
                    time: Time::WallclockHourMinuteTimezone {
                        clock: WallClockHour(NonZero::new(5).expect("valid")),
                        minute: Minute(53),
                        am: AmPm::Am,
                        timezone: TimezoneName("UTC".to_owned())
                    },
                    date: Date::Today
                }),
                actual
            )
        }

        #[test]
        fn timespec_time_date_harder() {
            let actual = Timespec::from_str("05:53amUTCNOV4,2024");

            assert_eq!(
                Ok(Timespec::TimeDate {
                    time: Time::WallclockHourMinuteTimezone {
                        clock: WallClockHour(NonZero::new(5).expect("valid")),
                        minute: Minute(53),
                        am: AmPm::Am,
                        timezone: TimezoneName("UTC".to_owned())
                    },
                    date: Date::MontDayYear {
                        month_name: Month(10),
                        day_number: DayNumber(NonZero::new(4).expect("valid")),
                        year_number: YearNumber(2024)
                    }
                }),
                actual
            )
        }

        #[test]
        fn timespec_time_date_harder_plus_increment() {
            let actual = Timespec::from_str("05:53amUTCNOV4,2024+1day");

            assert_eq!(
                Ok(Timespec::TimeDateIncrement {
                    time: Time::WallclockHourMinuteTimezone {
                        clock: WallClockHour(NonZero::new(5).expect("valid")),
                        minute: Minute(53),
                        am: AmPm::Am,
                        timezone: TimezoneName("UTC".to_owned())
                    },
                    date: Date::MontDayYear {
                        month_name: Month(10),
                        day_number: DayNumber(NonZero::new(4).expect("valid")),
                        year_number: YearNumber(2024)
                    },
                    inrement: Increment::Plus {
                        number: 1,
                        period: IncPeriod::Day
                    }
                }),
                actual
            )
        }

        #[test]
        fn timespec_time_date_harder_to_date_time() {
            let timespec = Timespec::TimeDate {
                time: Time::WallclockHourMinute {
                    clock: WallClockHour(NonZero::new(5).expect("valid")),
                    minute: Minute(53),
                    am: AmPm::Am,
                },
                date: Date::MontDayYear {
                    month_name: Month(10),
                    day_number: DayNumber(NonZero::new(4).expect("valid")),
                    year_number: YearNumber(3000),
                },
            };

            let expected = DateTime::parse_from_rfc3339("3000-11-04T05:53:00Z")
                .expect("expected is valid")
                .to_utc();

            assert_eq!(Some(expected), timespec.to_date_time())
        }

        #[test]
        fn timespec_time_date_harder_plus_increment_to_date_time() {
            let timespec = Timespec::TimeDateIncrement {
                time: Time::WallclockHourMinute {
                    clock: WallClockHour(NonZero::new(5).expect("valid")),
                    minute: Minute(53),
                    am: AmPm::Am,
                },
                date: Date::MontDayYear {
                    month_name: Month(10),
                    day_number: DayNumber(NonZero::new(4).expect("valid")),
                    year_number: YearNumber(3000),
                },
                inrement: Increment::Plus {
                    number: 1,
                    period: IncPeriod::Day,
                },
            };

            let expected = DateTime::parse_from_rfc3339("3000-11-05T05:53:00Z")
                .expect("expected is valid")
                .to_utc();

            assert_eq!(Some(expected), timespec.to_date_time())
        }
    }
}

mod tokens {
    use std::{num::NonZero, str::FromStr};

    #[derive(Debug, PartialEq)]
    pub enum TokenParsingError {
        TimeOperand(String),
        DateOperant(String),
        H24HourParsing {
            err: std::num::ParseIntError,
            input: String,
        },
        H24HourOverflow(&'static str),
        H24HourPatternNotFound(String),
        WallClockParsing {
            err: std::num::ParseIntError,
            input: String,
        },
        WallClockOverflow(&'static str),
        WallClockPatternNotFound(String),
        MinuteParsing {
            err: std::num::ParseIntError,
            input: String,
        },
        MinuteOverflow,
        DayNumberParsing {
            err: std::num::ParseIntError,
            input: String,
        },
        DayNumberOverflow,
        YearNumberParsing {
            err: std::num::ParseIntError,
            input: String,
        },
        YearNumberInvalid,
        TimezonePatternNotFound(String),
        MonthPatternNotFound(String),
        DayOfWeekPatternNotFound(String),
        AmPmPatternNotFound(String),
    }

    impl std::fmt::Display for TokenParsingError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                TokenParsingError::TimeOperand(input) => {
                    writeln!(f, "Failed to parse `time` operand in {input}")
                }
                TokenParsingError::DateOperant(input) => {
                    writeln!(f, "Failed to parse `date` operand in {input}")
                }
                TokenParsingError::H24HourParsing { err: _, input } => {
                    writeln!(f, "Failed to parse `hr24clock_hour` token in `{input}`")
                }
                TokenParsingError::H24HourOverflow(who) => writeln!(
                    f,
                    "Failed to parse `hr24clock_hour` token due overflow in `{who}`"
                ),
                TokenParsingError::H24HourPatternNotFound(input) => {
                    writeln!(f, "Failed to find `hr24clock_hour` token in `{input}`")
                }
                TokenParsingError::WallClockParsing { err: _, input } => {
                    writeln!(f, "Failed to parse `wallclock_hour` token in `{input}`")
                }
                TokenParsingError::WallClockOverflow(who) => writeln!(
                    f,
                    "Failed to parse `wallclock_hour` token due overflow in `{who}`"
                ),
                TokenParsingError::WallClockPatternNotFound(input) => {
                    writeln!(f, "Failed to find `wallclock_hour` token in `{input}`")
                }
                TokenParsingError::MinuteParsing { err: _, input } => {
                    writeln!(f, "Failed to parse `minute` token in `{input}`")
                }
                TokenParsingError::MinuteOverflow => {
                    writeln!(f, "Failed to parse `minute` token due overflow")
                }
                TokenParsingError::DayNumberParsing { err: _, input } => {
                    writeln!(f, "Failed to parse `day_number` token in `{input}`")
                }
                TokenParsingError::DayNumberOverflow => {
                    writeln!(f, "Failed to parse `day_number` token due overflow")
                }
                TokenParsingError::YearNumberParsing { err: _, input } => {
                    writeln!(f, "Failed to parse `year_number` token in `{input}`")
                }
                TokenParsingError::YearNumberInvalid => writeln!(
                    f,
                    "Failed to parse `year_number` token. Year should be 4 digit number"
                ),
                TokenParsingError::TimezonePatternNotFound(input) => {
                    writeln!(f, "Failed to find `timezone_name` token in `{input}`")
                }
                TokenParsingError::MonthPatternNotFound(input) => {
                    writeln!(f, "Failed to find `month_name` token in `{input}`")
                }
                TokenParsingError::DayOfWeekPatternNotFound(input) => {
                    writeln!(f, "Failed to find `day_of_week` token in `{input}`")
                }
                TokenParsingError::AmPmPatternNotFound(input) => {
                    writeln!(f, "Failed to find `am_pm` token in `{input}`")
                }
            }
        }
    }

    /// Specific operands in timespec for time
    pub enum TimespecTimeOperands {
        /// Indicates the time 12:00 am (00:00).
        Midnight,
        /// Indicates the time 12:00 pm.
        Noon,
        /// Indicates the current day and time.
        Now,
    }

    impl FromStr for TimespecTimeOperands {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let result = match s {
                "midnight" => Self::Midnight,
                "noon" => Self::Noon,
                "now" => Self::Now,
                _ => Err(TokenParsingError::TimeOperand(s.to_string()))?,
            };

            Ok(result)
        }
    }

    /// Specific operands in timespec for date
    pub enum TimespecDateOperands {
        /// Indicates the current day.
        Today,
        /// Indicates the day following the current day.
        Tomorrow,
    }

    impl FromStr for TimespecDateOperands {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let result = match s {
                "today" => Self::Today,
                "tomorrow" => Self::Tomorrow,
                _ => Err(TokenParsingError::DateOperant(s.to_owned()))?,
            };

            Ok(result)
        }
    }

    /// An `Hr24clockHour` is a one, two, or four-digit number. A one-digit
    /// or two-digit number constitutes an `Hr24clockHour`. An `Hr24clockHour`
    /// may be any of the single digits `[0,9]`, or may be double digits, ranging
    /// from `[00,23]`. If an `Hr24clockHour` is a four-digit number, the
    /// first two digits shall be a valid `Hr24clockHour`, while the last two
    /// represent the number of minutes, from `[00,59]`.
    #[derive(Debug, PartialEq)]
    pub struct Hr24Clock(pub(crate) [u8; 2]);

    #[cfg(test)]
    impl Hr24Clock {
        pub fn into_inner(self) -> [u8; 2] {
            self.0
        }
    }

    impl FromStr for Hr24Clock {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let chars_count = s.chars().count();

            let result = match chars_count {
                1 | 2 => {
                    let hour =
                        u8::from_str(s).map_err(|err| TokenParsingError::H24HourParsing {
                            err,
                            input: s.to_owned(),
                        })?;
                    if hour > 23 {
                        Err(TokenParsingError::H24HourOverflow("hour"))?
                    }

                    [hour, 0]
                }
                4 => {
                    // TODO: should be fine?
                    let hour = &s[..2];
                    let minutes = &s[2..];

                    let hour =
                        u8::from_str(hour).map_err(|err| TokenParsingError::H24HourParsing {
                            err,
                            input: s.to_owned(),
                        })?;
                    let minutes =
                        u8::from_str(minutes).map_err(|err| TokenParsingError::H24HourParsing {
                            err,
                            input: s.to_owned(),
                        })?;

                    if hour > 23 {
                        Err(TokenParsingError::H24HourOverflow("hour"))?
                    }

                    if minutes > 59 {
                        Err(TokenParsingError::H24HourOverflow("minute"))?
                    }

                    [hour, minutes]
                }
                _ => Err(TokenParsingError::H24HourPatternNotFound(s.to_owned()))?,
            };

            Ok(Self(result))
        }
    }

    #[derive(Debug, PartialEq)]
    pub struct Hr24ClockHour(pub(crate) u8);

    impl FromStr for Hr24ClockHour {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let number = u8::from_str(s).map_err(|err| TokenParsingError::H24HourParsing {
                err,
                input: s.to_owned(),
            })?;

            if number > 23 {
                Err(TokenParsingError::H24HourOverflow("hour"))?
            }

            Ok(Self(number))
        }
    }

    /// A `WallclockHour` is a one, two-digit, or four-digit number.
    /// A one-digit or two-digit number constitutes a `WallclockHour`.
    /// A `WallclockHour` may be any of the single digits `[1..=9]`, or may
    /// be double digits, ranging from `[01..=12]`. If a `WallclockHour`
    /// is a four-digit number, the first two digits shall be a valid
    /// `WallclockHour`, while the last two represent the number of
    /// minutes, from `[00..=59]`.
    #[derive(Debug, PartialEq)]
    pub struct WallClock {
        pub(crate) hour: NonZero<u8>,
        pub(crate) minutes: u8,
    }

    #[cfg(test)]
    impl WallClock {
        pub fn into_inner(self) -> (NonZero<u8>, u8) {
            (self.hour, self.minutes)
        }
    }

    impl FromStr for WallClock {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let chars_count = s.chars().count();

            let result = match chars_count {
                1 | 2 => {
                    let hour = NonZero::<u8>::from_str(s).map_err(|err| {
                        TokenParsingError::WallClockParsing {
                            err,
                            input: s.to_owned(),
                        }
                    })?;

                    if hour.get() > 23 {
                        Err(TokenParsingError::WallClockOverflow("hour"))?
                    }

                    Self { hour, minutes: 0 }
                }
                4 => {
                    let hour = &s[..2];
                    let minutes = &s[2..];

                    let hour = NonZero::<u8>::from_str(hour).map_err(|err| {
                        TokenParsingError::WallClockParsing {
                            err,
                            input: s.to_owned(),
                        }
                    })?;
                    let minutes = u8::from_str(minutes).map_err(|err| {
                        TokenParsingError::WallClockParsing {
                            err,
                            input: s.to_owned(),
                        }
                    })?;

                    if hour.get() > 23 {
                        Err(TokenParsingError::WallClockOverflow("hour"))?
                    }

                    if minutes > 59 {
                        Err(TokenParsingError::WallClockOverflow("minute"))?
                    }

                    Self { hour, minutes }
                }
                _ => Err(TokenParsingError::WallClockPatternNotFound(s.to_owned()))?,
            };

            Ok(result)
        }
    }

    #[derive(Debug, PartialEq)]
    pub struct WallClockHour(pub(crate) NonZero<u8>);

    impl FromStr for WallClockHour {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let number =
                NonZero::from_str(s).map_err(|err| TokenParsingError::WallClockParsing {
                    err,
                    input: s.to_owned(),
                })?;

            if number.get() > 12 {
                Err(TokenParsingError::WallClockOverflow("hour"))?
            }

            Ok(Self(number))
        }
    }

    /// A `Minute` is a one or two-digit number whose value
    /// can be `[0..=9]` or` [00..=59]`.
    #[derive(Debug, PartialEq)]
    pub struct Minute(pub(crate) u8);

    #[cfg(test)]
    impl Minute {
        pub fn into_inner(self) -> u8 {
            self.0
        }
    }

    impl FromStr for Minute {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let minute = u8::from_str(s).map_err(|err| TokenParsingError::MinuteParsing {
                err,
                input: s.to_owned(),
            })?;

            if minute > 59 {
                Err(TokenParsingError::MinuteOverflow)?
            }

            Ok(Self(minute))
        }
    }

    #[derive(Debug, PartialEq)]
    pub struct DayNumber(pub(crate) NonZero<u8>);

    impl FromStr for DayNumber {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let number =
                NonZero::from_str(s).map_err(|err| TokenParsingError::DayNumberParsing {
                    err,
                    input: s.to_owned(),
                })?;

            if number.get() > 31 {
                Err(TokenParsingError::DayNumberOverflow)?;
            }

            Ok(Self(number))
        }
    }

    /// A `YearNumber` is a four-digit number representing the year A.D., in
    /// which the at_job is to be run.
    #[derive(Debug, PartialEq)]
    pub struct YearNumber(pub(crate) u16);

    impl FromStr for YearNumber {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let year = u16::from_str(s).map_err(|err| TokenParsingError::YearNumberParsing {
                err,
                input: s.to_owned(),
            })?;
            if year < 1970 || year > 9999 {
                // it should be 4 number, so yeah...
                Err(TokenParsingError::YearNumberInvalid)?
            }

            Ok(Self(year))
        }
    }

    /// The name of an optional timezone suffix to the time field, in an
    /// implementation-defined format.
    #[derive(Debug, PartialEq)]
    pub struct TimezoneName(pub(crate) String);

    impl FromStr for TimezoneName {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let tz = std::env::var("TZ").ok().unwrap_or("UTC".to_owned());

            match s == &tz {
                true => Ok(Self(tz)), // TODO: Seems like implementation only reads UTC, but it should be influenced by TZ variable
                false => Err(TokenParsingError::TimezonePatternNotFound(tz)),
            }
        }
    }

    /// One of the values from the mon or abmon keywords in the LC_TIME
    /// locale category.
    #[derive(Debug, PartialEq)]
    pub struct Month(pub(crate) u8);

    impl FromStr for Month {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let number = match s {
                "JAN" => 0,
                "FEB" => 1,
                "MAR" => 2,
                "APR" => 3,
                "MAY" => 4,
                "JUN" => 5,
                "JUL" => 6,
                "AUG" => 7,
                "SEP" => 8,
                "OCT" => 9,
                "NOV" => 10,
                "DEC" => 11,
                _ => Err(TokenParsingError::MonthPatternNotFound(s.to_owned()))?,
            };

            Ok(Self(number))
        }
    }

    /// One of the values from the day or abday keywords in the LC_TIME
    /// locale category.
    #[derive(Debug, PartialEq)]
    pub struct DayOfWeek(pub(crate) chrono::Weekday);

    impl FromStr for DayOfWeek {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let day = match s {
                "SUN" => chrono::Weekday::Sun,
                "MON" => chrono::Weekday::Mon,
                "TUE" => chrono::Weekday::Tue,
                "WED" => chrono::Weekday::Wed,
                "THU" => chrono::Weekday::Thu,
                "FRI" => chrono::Weekday::Fri,
                "SAT" => chrono::Weekday::Sat,
                _ => Err(TokenParsingError::DayOfWeekPatternNotFound(s.to_owned()))?,
            };

            Ok(Self(day))
        }
    }

    /// One of the values from the am_pm keyword in the LC_TIME locale
    /// category.
    #[derive(Debug, PartialEq, Clone, Copy)]
    pub enum AmPm {
        Am,
        Pm,
    }

    impl FromStr for AmPm {
        type Err = TokenParsingError;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            Ok(match s.to_lowercase().as_str() {
                "am" => Self::Am,
                "pm" => Self::Pm,
                _ => Err(TokenParsingError::AmPmPatternNotFound(s.to_owned()))?,
            })
        }
    }

    #[cfg(test)]
    mod test {
        use super::*;

        #[test]
        fn hour24_empty_char() {
            let actual = Hr24Clock::from_str("").map(Hr24Clock::into_inner);

            assert_eq!(
                Err(TokenParsingError::H24HourPatternNotFound("".to_owned())),
                actual
            )
        }

        #[test]
        fn hour24_single_char_ok() {
            for value in 0..=9 {
                let actual = Hr24Clock::from_str(&value.to_string()).map(Hr24Clock::into_inner);

                assert_eq!(Ok([value, 0,]), actual)
            }
        }

        #[test]
        fn hour24_single_char_not_a_number() {
            let actual = Hr24Clock::from_str("a").map(Hr24Clock::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn hour24_two_chars_ok() {
            for hour in 10..23 {
                let actual = Hr24Clock::from_str(&hour.to_string()).map(Hr24Clock::into_inner);

                assert_eq!(Ok([hour, 0]), actual)
            }
        }

        #[test]
        fn hour24_two_chars_out_of_range() {
            let actual = Hr24Clock::from_str(&format!("24")).map(Hr24Clock::into_inner);

            assert_eq!(Err(TokenParsingError::H24HourOverflow("hour")), actual)
        }

        #[test]
        fn hour24_two_chars_not_a_number() {
            let actual = Hr24Clock::from_str(&format!("aa")).map(Hr24Clock::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn hour24_four_chars() {
            for hour in 10..23 {
                for minute in 0..10 {
                    let actual =
                        Hr24Clock::from_str(&format!("{hour}0{minute}")).map(Hr24Clock::into_inner);

                    assert_eq!(Ok([hour, minute]), actual)
                }

                for minute in 10..=59 {
                    let actual =
                        Hr24Clock::from_str(&format!("{hour}{minute}")).map(Hr24Clock::into_inner);

                    assert_eq!(Ok([hour, minute]), actual)
                }
            }
        }

        #[test]
        fn hour24_four_chars_out_of_range() {
            let actual = Hr24Clock::from_str(&format!("2400")).map(Hr24Clock::into_inner);

            assert_eq!(Err(TokenParsingError::H24HourOverflow("hour")), actual);

            let actual = Hr24Clock::from_str(&format!("2360")).map(Hr24Clock::into_inner);

            assert_eq!(Err(TokenParsingError::H24HourOverflow("minute")), actual);
        }

        #[test]
        fn hour24_four_chars_not_a_number() {
            let actual = Hr24Clock::from_str(&format!("aaaa")).map(Hr24Clock::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn wallclock_hour_empty_char() {
            let actual = WallClock::from_str("").map(WallClock::into_inner);

            assert_eq!(
                Err(TokenParsingError::WallClockPatternNotFound("".to_owned())),
                actual
            )
        }

        #[test]
        fn wallclock_hour_single_char_ok() {
            for value in 1..=9 {
                let actual = WallClock::from_str(&value.to_string()).map(WallClock::into_inner);

                assert_eq!(
                    Ok((NonZero::new(value).expect("not a zero"), 0_u8,)),
                    actual
                )
            }
        }

        #[test]
        fn wallclock_hour_single_char_not_a_number() {
            let actual = WallClock::from_str("a").map(WallClock::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn wallclock_hour_two_chars_ok() {
            for hour in 10..23 {
                let actual = WallClock::from_str(&hour.to_string()).map(WallClock::into_inner);

                assert_eq!(Ok((NonZero::new(hour).expect("not a zero"), 0)), actual)
            }
        }

        #[test]
        fn wallclock_hour_two_chars_out_of_range() {
            let actual = WallClock::from_str("24").map(WallClock::into_inner);

            assert_eq!(Err(TokenParsingError::WallClockOverflow("hour")), actual)
        }

        #[test]
        fn wallclock_hour_two_chars_not_a_number() {
            let actual = WallClock::from_str("aa").map(WallClock::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn wallclock_hour_four_chars() {
            for hour in 10..23 {
                for minute in 0..10 {
                    let actual =
                        WallClock::from_str(&format!("{hour}0{minute}")).map(WallClock::into_inner);

                    assert_eq!(
                        Ok((NonZero::new(hour).expect("not a zero"), minute)),
                        actual
                    )
                }

                for minute in 10..=59 {
                    let actual =
                        WallClock::from_str(&format!("{hour}{minute}")).map(WallClock::into_inner);

                    assert_eq!(
                        Ok((NonZero::new(hour).expect("not a zero"), minute)),
                        actual
                    )
                }
            }
        }

        #[test]
        fn wallclock_hour_four_chars_out_of_range() {
            let actual = WallClock::from_str(&format!("2400")).map(WallClock::into_inner);

            assert_eq!(Err(TokenParsingError::WallClockOverflow("hour")), actual);

            let actual = WallClock::from_str(&format!("2360")).map(WallClock::into_inner);

            assert_eq!(Err(TokenParsingError::WallClockOverflow("minute")), actual);
        }

        #[test]
        fn wallclock_hour_four_chars_not_a_number() {
            let actual = WallClock::from_str("aaaa").map(WallClock::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn wallclock_hour_singe_char_zero_fails() {
            let actual = WallClock::from_str("0").map(WallClock::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn wallclock_hour_two_chars_zero_fails() {
            let actual = WallClock::from_str("00").map(WallClock::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn wallclock_hour_four_chars_zero_fails() {
            let actual = WallClock::from_str("0035").map(WallClock::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn minute_empty_char() {
            let actual = Minute::from_str("").map(Minute::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn minute_single_char_ok() {
            for value in 1..=9 {
                let actual = Minute::from_str(&value.to_string()).map(Minute::into_inner);

                assert_eq!(Ok(value), actual)
            }
        }

        #[test]
        fn minute_single_char_not_a_number() {
            let actual = Minute::from_str("a").map(Minute::into_inner);

            assert!(actual.is_err())
        }

        #[test]
        fn minute_two_chars_ok() {
            for value in 10..=59 {
                let actual = Minute::from_str(&value.to_string()).map(Minute::into_inner);

                assert_eq!(Ok(value), actual)
            }
        }

        #[test]
        fn minute_two_chars_out_of_range() {
            let actual = Minute::from_str("60").map(Minute::into_inner);

            assert_eq!(Err(TokenParsingError::MinuteOverflow), actual)
        }

        #[test]
        fn minute_two_chars_not_a_number() {
            let actual = Minute::from_str("aa").map(Minute::into_inner);

            assert!(actual.is_err())
        }
    }
}
