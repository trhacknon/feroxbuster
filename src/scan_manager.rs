use crate::config::Configuration;
use crate::reporter::safe_file_write;
use crate::utils::open_file;
use crate::{
    config::{CONFIGURATION, PROGRESS_PRINTER},
    progress,
    scanner::{NUMBER_OF_REQUESTS, RESPONSES, SCANNED_URLS},
    FeroxResponse, FeroxSerialize, SLEEP_DURATION,
};
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
use serde::ser::SerializeSeq;
use serde::{ser::SerializeStruct, Deserialize, Serialize, Serializer};
use std::{
    cmp::PartialEq,
    fmt,
    fs::File,
    io::BufReader,
    sync::{Arc, Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use std::{
    io::{stderr, Write},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::{task::JoinHandle, time};
use uuid::Uuid;

lazy_static! {
    /// A clock spinner protected with a RwLock to allow for a single thread to use at a time
    // todo remove this when issue #107 is resolved
    static ref SINGLE_SPINNER: RwLock<ProgressBar> = RwLock::new(get_single_spinner());
}

/// Single atomic number that gets incremented once, used to track first thread to interact with
/// when pausing a scan
static INTERACTIVE_BARRIER: AtomicUsize = AtomicUsize::new(0);

/// Atomic boolean flag, used to determine whether or not a scan should pause or resume
pub static PAUSE_SCAN: AtomicBool = AtomicBool::new(false);

/// Simple enum used to flag a `FeroxScan` as likely a directory or file
#[derive(Debug, Serialize, Deserialize)]
pub enum ScanType {
    File,
    Directory,
}

/// Struct to hold scan-related state
///
/// The purpose of this container is to open up the pathway to aborting currently running tasks and
/// serialization of all scan state into a state file in order to resume scans that were cut short
#[derive(Debug)]
pub struct FeroxScan {
    /// UUID that uniquely ID's the scan
    pub id: String,

    /// The URL that to be scanned
    pub url: String,

    /// The type of scan
    pub scan_type: ScanType,

    /// Whether or not this scan has completed
    pub complete: bool,

    /// The spawned tokio task performing this scan
    pub task: Option<JoinHandle<()>>,

    /// The progress bar associated with this scan
    pub progress_bar: Option<ProgressBar>,
}

/// Implementation of FeroxScan
impl FeroxScan {
    /// Stop a currently running scan
    pub fn abort(&self) {
        self.stop_progress_bar();

        if let Some(_task) = &self.task {
            // task.abort();  todo uncomment once upgraded to tokio 0.3 (issue #107)
        }
    }

    /// Create a default FeroxScan, populates ID with a new UUID
    fn default() -> Self {
        let new_id = Uuid::new_v4().to_simple().to_string();

        FeroxScan {
            id: new_id,
            task: None,
            complete: false,
            url: String::new(),
            progress_bar: None,
            scan_type: ScanType::File,
        }
    }

    /// Simple helper to call .finish on the scan's progress bar
    fn stop_progress_bar(&self) {
        if let Some(pb) = &self.progress_bar {
            pb.finish();
        }
    }

    /// Simple helper get a progress bar
    pub fn progress_bar(&mut self) -> ProgressBar {
        if let Some(pb) = &self.progress_bar {
            pb.clone()
        } else {
            let num_requests = NUMBER_OF_REQUESTS.load(Ordering::Relaxed);
            let pb = progress::add_bar(&self.url, num_requests, false);

            pb.reset_elapsed();

            self.progress_bar = Some(pb.clone());

            pb
        }
    }

    /// Given a URL and ProgressBar, create a new FeroxScan, wrap it in an Arc and return it
    pub fn new(url: &str, scan_type: ScanType, pb: Option<ProgressBar>) -> Arc<Mutex<Self>> {
        let mut me = Self::default();

        me.url = url.to_string();
        me.scan_type = scan_type;
        me.progress_bar = pb;

        Arc::new(Mutex::new(me))
    }

    /// Mark the scan as complete and stop the scan's progress bar
    pub fn finish(&mut self) {
        self.complete = true;
        self.stop_progress_bar();
    }
}

/// Display implementation
impl fmt::Display for FeroxScan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let complete = if self.complete {
            style("complete").green()
        } else {
            style("incomplete").red()
        };

        write!(f, "{:10} {}", complete, self.url)
    }
}

/// PartialEq implementation; uses FeroxScan.id for comparison
impl PartialEq for FeroxScan {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

/// Serialize implementation for FeroxScan
impl Serialize for FeroxScan {
    /// Function that handles serialization of a FeroxScan
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("FeroxScan", 4)?;

        state.serialize_field("id", &self.id)?;
        state.serialize_field("url", &self.url)?;
        state.serialize_field("scan_type", &self.scan_type)?;
        state.serialize_field("complete", &self.complete)?;

        state.end()
    }
}

/// Container around a locked hashset of `FeroxScan`s, adds wrappers for insertion and searching
#[derive(Debug, Default)]
pub struct FeroxScans {
    /// Internal structure: locked hashset of `FeroxScan`s
    pub scans: Mutex<Vec<Arc<Mutex<FeroxScan>>>>,
}

/// Serialize implementation for FeroxScans
impl Serialize for FeroxScans {
    /// Function that handles serialization of FeroxScans
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Ok(scans) = self.scans.lock() {
            let mut seq = serializer.serialize_seq(Some(scans.len()))?;

            for scan in scans.iter() {
                if let Ok(unlocked) = scan.lock() {
                    seq.serialize_element(&*unlocked)?;
                }
            }

            seq.end()
        } else {
            // if for some reason we can't unlock the mutex, just write an empty list
            let seq = serializer.serialize_seq(Some(0))?;
            seq.end()
        }
    }
}

/// Implementation of `FeroxScans`
impl FeroxScans {
    /// Add a `FeroxScan` to the internal container
    ///
    /// If the internal container did NOT contain the scan, true is returned; else false
    pub fn insert(&self, scan: Arc<Mutex<FeroxScan>>) -> bool {
        let sentry = match scan.lock() {
            Ok(locked_scan) => {
                // If the container did contain the scan, set sentry to false
                // If the container did not contain the scan, set sentry to true
                !self.contains(&locked_scan.url)
            }
            Err(e) => {
                // poisoned lock
                log::error!("FeroxScan's ({:?}) mutex is poisoned: {}", self, e);
                false
            }
        };

        if sentry {
            // can't update the internal container while the scan itself is locked, so first
            // lock the scan and check the container for the scan's presence, then add if
            // not found
            match self.scans.lock() {
                Ok(mut scans) => {
                    scans.push(scan);
                }
                Err(e) => {
                    log::error!("FeroxScans' container's mutex is poisoned: {}", e);
                    return false;
                }
            }
        }

        sentry
    }

    /// Simple check for whether or not a FeroxScan is contained within the inner container based
    /// on the given URL
    pub fn contains(&self, url: &str) -> bool {
        match self.scans.lock() {
            Ok(scans) => {
                for scan in scans.iter() {
                    if let Ok(locked_scan) = scan.lock() {
                        if locked_scan.url == url {
                            return true;
                        }
                    }
                }
            }
            Err(e) => {
                log::error!("FeroxScans' container's mutex is poisoned: {}", e);
            }
        }
        false
    }

    /// Find and return a `FeroxScan` based on the given URL
    pub fn get_scan_by_url(&self, url: &str) -> Option<Arc<Mutex<FeroxScan>>> {
        if let Ok(scans) = self.scans.lock() {
            for scan in scans.iter() {
                if let Ok(locked_scan) = scan.lock() {
                    if locked_scan.url == url {
                        return Some(scan.clone());
                    }
                }
            }
        }
        None
    }

    /// Print all FeroxScans of type Directory
    ///
    /// Example:
    ///   0: complete   https://10.129.45.20
    ///   9: complete   https://10.129.45.20/images
    ///  10: complete   https://10.129.45.20/assets
    pub fn display_scans(&self) {
        if let Ok(scans) = self.scans.lock() {
            for (i, scan) in scans.iter().enumerate() {
                if let Ok(unlocked_scan) = scan.lock() {
                    match unlocked_scan.scan_type {
                        ScanType::Directory => {
                            PROGRESS_PRINTER.println(format!("{:3}: {}", i, unlocked_scan));
                        }
                        ScanType::File => {
                            // we're only interested in displaying directory scans, as those are
                            // the only ones that make sense to be stopped
                        }
                    }
                }
            }
        }
    }

    /// Forced the calling thread into a busy loop
    ///
    /// Every `SLEEP_DURATION` milliseconds, the function examines the result stored in `PAUSE_SCAN`
    ///
    /// When the value stored in `PAUSE_SCAN` becomes `false`, the function returns, exiting the busy
    /// loop
    pub async fn pause(&self, get_user_input: bool) {
        // function uses tokio::time, not std

        // local testing showed a pretty slow increase (less than linear) in CPU usage as # of
        // concurrent scans rose when SLEEP_DURATION was set to 500, using that as the default for now
        let mut interval = time::interval(time::Duration::from_millis(SLEEP_DURATION));

        // ignore any error returned
        let _ = stderr().flush();

        if INTERACTIVE_BARRIER.load(Ordering::Relaxed) == 0 {
            INTERACTIVE_BARRIER.fetch_add(1, Ordering::Relaxed);

            if get_user_input {
                self.display_scans();

                let mut user_input = String::new();
                std::io::stdin().read_line(&mut user_input).unwrap();
                // todo (issue #107) actual logic for parsing user input in a way that allows for
                // calling .abort on the scan retrieved based on the input
            }
        }

        if SINGLE_SPINNER.read().unwrap().is_finished() {
            // todo remove this when issue #107 is resolved

            // in order to not leave draw artifacts laying around in the terminal, we call
            // finish_and_clear on the progress bar when resuming scans. For this reason, we need to
            // check if the spinner is finished, and repopulate the RwLock with a new spinner if
            // necessary
            if let Ok(mut guard) = SINGLE_SPINNER.write() {
                *guard = get_single_spinner();
            }
        }

        if let Ok(spinner) = SINGLE_SPINNER.write() {
            spinner.enable_steady_tick(120);
        }

        loop {
            // first tick happens immediately, all others wait the specified duration
            interval.tick().await;

            if !PAUSE_SCAN.load(Ordering::Acquire) {
                // PAUSE_SCAN is false, so we can exit the busy loop

                if INTERACTIVE_BARRIER.load(Ordering::Relaxed) == 1 {
                    INTERACTIVE_BARRIER.fetch_sub(1, Ordering::Relaxed);
                }

                if let Ok(spinner) = SINGLE_SPINNER.write() {
                    // todo remove this when issue #107 is resolved
                    spinner.finish_and_clear();
                }

                let _ = stderr().flush();

                log::trace!("exit: pause_scan");
                return;
            }
        }
    }

    /// Given a url, create a new `FeroxScan` and add it to `FeroxScans`
    ///
    /// If `FeroxScans` did not already contain the scan, return true; otherwise return false
    ///
    /// Also return a reference to the new `FeroxScan`
    fn add_scan(&self, url: &str, scan_type: ScanType) -> (bool, Arc<Mutex<FeroxScan>>) {
        let bar = match scan_type {
            ScanType::Directory => {
                let progress_bar =
                    progress::add_bar(&url, NUMBER_OF_REQUESTS.load(Ordering::Relaxed), false);

                progress_bar.reset_elapsed();

                Some(progress_bar)
            }
            ScanType::File => None,
        };

        let ferox_scan = FeroxScan::new(&url, scan_type, bar);

        // If the set did not contain the scan, true is returned.
        // If the set did contain the scan, false is returned.
        let response = self.insert(ferox_scan.clone());

        (response, ferox_scan)
    }

    /// Given a url, create a new `FeroxScan` and add it to `FeroxScans` as a Directory Scan
    ///
    /// If `FeroxScans` did not already contain the scan, return true; otherwise return false
    ///
    /// Also return a reference to the new `FeroxScan`
    pub fn add_directory_scan(&self, url: &str) -> (bool, Arc<Mutex<FeroxScan>>) {
        self.add_scan(&url, ScanType::Directory)
    }

    /// Given a url, create a new `FeroxScan` and add it to `FeroxScans` as a File Scan
    ///
    /// If `FeroxScans` did not already contain the scan, return true; otherwise return false
    ///
    /// Also return a reference to the new `FeroxScan`
    pub fn add_file_scan(&self, url: &str) -> (bool, Arc<Mutex<FeroxScan>>) {
        self.add_scan(&url, ScanType::File)
    }
}

/// Return a clock spinner, used when scans are paused
// todo remove this when issue #107 is resolved
fn get_single_spinner() -> ProgressBar {
    log::trace!("enter: get_single_spinner");

    let spinner = ProgressBar::new_spinner().with_style(
        ProgressStyle::default_spinner()
            .tick_strings(&[
                "🕛", "🕐", "🕑", "🕒", "🕓", "🕔", "🕕", "🕖", "🕗", "🕘", "🕙", "🕚",
            ])
            .template(&format!(
                "\t-= All Scans {{spinner}} {} =-",
                style("Paused").red()
            )),
    );

    log::trace!("exit: get_single_spinner -> {:?}", spinner);
    spinner
}

/// Container around a locked vector of `FeroxResponse`s, adds wrappers for insertion and search
#[derive(Debug, Default)]
pub struct FeroxResponses {
    /// Internal structure: locked hashset of `FeroxScan`s
    pub responses: Arc<RwLock<Vec<FeroxResponse>>>,
}

/// Serialize implementation for FeroxResponses
impl Serialize for FeroxResponses {
    /// Function that handles serialization of FeroxResponses
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Ok(responses) = self.responses.read() {
            let mut seq = serializer.serialize_seq(Some(responses.len()))?;

            for response in responses.iter() {
                seq.serialize_element(response)?;
            }

            seq.end()
        } else {
            // if for some reason we can't unlock the mutex, just write an empty list
            let seq = serializer.serialize_seq(Some(0))?;
            seq.end()
        }
    }
}

/// Implementation of `FeroxResponses`
impl FeroxResponses {
    /// Add a `FeroxResponse` to the internal container
    pub fn insert(&self, response: FeroxResponse) {
        match self.responses.write() {
            Ok(mut responses) => {
                responses.push(response);
            }
            Err(e) => {
                log::error!("FeroxResponses' container's mutex is poisoned: {}", e);
            }
        }
    }

    /// Simple check for whether or not a FeroxResponse is contained within the inner container
    pub fn contains(&self, other: &FeroxResponse) -> bool {
        match self.responses.read() {
            Ok(responses) => {
                for response in responses.iter() {
                    if response.url == other.url {
                        return true;
                    }
                }
            }
            Err(e) => {
                log::error!("FeroxResponses' container's mutex is poisoned: {}", e);
            }
        }
        false
    }
}
/// Data container for (de)?serialization of multiple items
#[derive(Serialize, Debug)]
pub struct FeroxState {
    /// Known scans
    scans: &'static FeroxScans,

    /// Current running config
    config: &'static Configuration,

    /// Known responses
    responses: &'static FeroxResponses,
}

/// todo doc
impl FeroxSerialize for FeroxState {
    /// todo doc
    fn as_str(&self) -> String {
        format!("{:?}", self)
    }

    /// todo doc
    fn as_json(&self) -> String {
        // todo unwrap
        serde_json::to_string(&self).unwrap()
    }
}

/// Initialize the ctrl+c handler that saves scan state to disk
pub fn initialize() {
    log::trace!("enter: initialize");

    let result = ctrlc::set_handler(move || {
        let filename = format!(
            "ferox-{}.state",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
        let warning = format!(
            "🚨 Caught {} 🚨 saving scan state to {} ...",
            style("ctrl+c").yellow(),
            filename
        );

        PROGRESS_PRINTER.println(warning);

        let state = FeroxState {
            config: &CONFIGURATION,
            scans: &SCANNED_URLS,
            responses: &RESPONSES,
        };

        let state_file = open_file(&filename);

        if let Some(buffered_file) = state_file {
            safe_file_write(&state, buffered_file, true);
        }

        std::process::exit(1);
    });

    if result.is_err() {
        log::error!("Could not set Ctrl+c handler");
        std::process::exit(1);
    }

    log::trace!("exit: initialize");
}

/// todo doc
pub fn resume_scan(filename: &str) -> Configuration {
    log::trace!("enter: resume_scan({})", filename);

    let file = File::open(filename).unwrap();
    let reader = BufReader::new(file);
    let state: serde_json::Value = serde_json::from_reader(reader).unwrap();

    // todo unwrap
    let config: Configuration =
        serde_json::from_value(state.get("config").unwrap().clone()).unwrap();
    // let scans: FeroxScans = serde_json::from_value(state.get("scans").unwrap().clone()).unwrap();
    let responses = state.get("responses").unwrap().as_array().unwrap();

    for response in responses {
        let response: FeroxResponse = serde_json::from_value(response.clone()).unwrap();
        RESPONSES.insert(response);
    }

    println!("STATE CONFIGURATION: {:?}\n", config);
    println!("STATE RESPONSES: {:?}\n", *RESPONSES);

    // println!("STATE: {:?}", state.get("config").unwrap().get("add_slash").unwrap().as_bool().unwrap());
    // println!("STATE: {:?}\n\n", scans);
    // println!("STATE: {:?}", state.get("responses"));

    log::trace!("exit: resume_scan -> {:?}", config);
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// test that get_single_spinner returns the correct spinner
    // todo remove this when issue #107 is resolved
    fn scanner_get_single_spinner_returns_spinner() {
        let spinner = get_single_spinner();
        assert!(!spinner.is_finished());
    }

    #[tokio::test(core_threads = 1)]
    /// tests that pause_scan pauses execution and releases execution when PAUSE_SCAN is toggled
    /// the spinner used during the test has had .finish_and_clear called on it, meaning that
    /// a new one will be created, taking the if branch within the function
    async fn scanner_pause_scan_with_finished_spinner() {
        let now = time::Instant::now();
        let urls = FeroxScans::default();

        PAUSE_SCAN.store(true, Ordering::Relaxed);

        let expected = time::Duration::from_secs(2);

        tokio::spawn(async move {
            time::delay_for(expected).await;
            PAUSE_SCAN.store(false, Ordering::Relaxed);
        });

        urls.pause(false).await;

        assert!(now.elapsed() > expected);
    }

    #[test]
    /// add an unknown url to the hashset, expect true
    fn add_url_to_list_of_scanned_urls_with_unknown_url() {
        let urls = FeroxScans::default();
        let url = "http://unknown_url";
        let (result, _scan) = urls.add_scan(url, ScanType::Directory);
        assert_eq!(result, true);
    }

    #[test]
    /// add a known url to the hashset, with a trailing slash, expect false
    fn add_url_to_list_of_scanned_urls_with_known_url() {
        let urls = FeroxScans::default();
        let pb = ProgressBar::new(1);
        let url = "http://unknown_url/";
        let scan = FeroxScan::new(url, ScanType::Directory, Some(pb));

        assert_eq!(urls.insert(scan), true);

        let (result, _scan) = urls.add_scan(url, ScanType::Directory);

        assert_eq!(result, false);
    }

    #[test]
    /// abort should call stop_progress_bar, marking it as finished
    fn abort_stops_progress_bar() {
        let pb = ProgressBar::new(1);
        let url = "http://unknown_url/";
        let scan = FeroxScan::new(url, ScanType::Directory, Some(pb));

        assert_eq!(
            scan.lock()
                .unwrap()
                .progress_bar
                .as_ref()
                .unwrap()
                .is_finished(),
            false
        );

        scan.lock().unwrap().abort();

        assert_eq!(
            scan.lock()
                .unwrap()
                .progress_bar
                .as_ref()
                .unwrap()
                .is_finished(),
            true
        );
    }

    #[test]
    /// add a known url to the hashset, without a trailing slash, expect false
    fn add_url_to_list_of_scanned_urls_with_known_url_without_slash() {
        let urls = FeroxScans::default();
        let url = "http://unknown_url";
        let scan = FeroxScan::new(url, ScanType::File, None);

        assert_eq!(urls.insert(scan), true);

        let (result, _scan) = urls.add_scan(url, ScanType::File);

        assert_eq!(result, false);
    }

    #[test]
    /// just increasing coverage, no real expectations
    fn call_display_scans() {
        let urls = FeroxScans::default();
        let pb = ProgressBar::new(1);
        let pb_two = ProgressBar::new(2);
        let url = "http://unknown_url/";
        let url_two = "http://unknown_url/fa";
        let scan = FeroxScan::new(url, ScanType::Directory, Some(pb));
        let scan_two = FeroxScan::new(url_two, ScanType::Directory, Some(pb_two));

        scan_two.lock().unwrap().finish(); // one complete, one incomplete

        assert_eq!(urls.insert(scan), true);

        urls.display_scans();
    }

    #[test]
    /// ensure that PartialEq compares FeroxScan.id fields
    fn partial_eq_compares_the_id_field() {
        let url = "http://unknown_url/";
        let scan = FeroxScan::new(url, ScanType::Directory, None);
        let scan_two = FeroxScan::new(url, ScanType::Directory, None);

        assert!(!scan.lock().unwrap().eq(&scan_two.lock().unwrap()));

        scan_two.lock().unwrap().id = scan.lock().unwrap().id.clone();

        assert!(scan.lock().unwrap().eq(&scan_two.lock().unwrap()));
    }

    #[test]
    /// show that a new progress bar is created if one doesn't exist
    fn ferox_scan_get_progress_bar_when_none_is_set() {
        let mut scan = FeroxScan::default();

        assert!(scan.progress_bar.is_none()); // no pb exists

        let pb = scan.progress_bar();

        assert!(scan.progress_bar.is_some()); // new pb created
        assert!(!pb.is_finished()) // not finished
    }
}
