#[macro_use]
extern crate log;
#[macro_use]
extern crate clap;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate lazy_static;

use crossterm::{
	event::{
		self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyModifiers,
		MouseEvent,
	},
	execute,
	terminal::LeaveAlternateScreen,
	terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen},
};

use std::{
	io::{stdout, Write},
	sync::mpsc,
	thread,
	time::{Duration, Instant},
};
use tui::{backend::CrosstermBackend, Terminal};

pub mod app;
mod utils {
	pub mod error;
	pub mod gen_util;
	pub mod logging;
}
mod canvas;
mod constants;
mod data_conversion;

use app::data_harvester;
use app::data_harvester::processes::ProcessHarvest;
use constants::TICK_RATE_IN_MILLISECONDS;
use data_conversion::*;
use std::collections::BTreeMap;
use utils::error::{self, BottomError};

enum Event<I, J> {
	KeyInput(I),
	MouseInput(J),
	Update(Box<data_harvester::Data>),
}

enum ResetEvent {
	Reset,
}

fn main() -> error::Result<()> {
	// Parse command line options
	let matches = clap_app!(app =>
		(name: crate_name!())
		(version: crate_version!())
		(author: crate_authors!())
		(about: crate_description!())
		(@arg AVG_CPU: -a --avgcpu "Enables showing the average CPU usage.")
		(@arg DOT_MARKER: -m --dot_marker "Use a dot marker instead of the default braille marker.")
		(@arg DEBUG: -d --debug "Enables debug mode, which will output a log file.")
		(@group TEMPERATURE_TYPE =>
			(@arg CELSIUS : -c --celsius "Sets the temperature type to Celsius.  This is the default option.")
			(@arg FAHRENHEIT : -f --fahrenheit "Sets the temperature type to Fahrenheit.")
			(@arg KELVIN : -k --kelvin "Sets the temperature type to Kelvin.")
		)
		(@arg RATE_MILLIS: -r --rate +takes_value "Sets a refresh rate in milliseconds; the minimum is 250ms, defaults to 1000ms.  Smaller values may take more resources.")
		(@arg LEFT_LEGEND: -l --left_legend "Puts external chart legends on the left side rather than the default right side.")
		(@arg USE_CURR_USAGE: -u --current_usage "Within Linux, sets a process' CPU usage to be based on the total current CPU usage, rather than assuming 100% usage.")
		//(@arg CONFIG_LOCATION: -co --config +takes_value "Sets the location of the config file.  Expects a config file in the JSON format.")
		(@arg BASIC_MODE: -b --basic "Sets bottom to basic mode, not showing graphs and only showing basic tables.")
		(@arg GROUP_PROCESSES: -g --group "Groups processes with the same name together on launch.")
		(@arg SEARCH_DEFAULT_USE_SIMPLE: -s --simple_search "Uses a simple case-insensitive string comparison to search processes by default.")
	)
	.get_matches();

	let update_rate_in_milliseconds: u128 = if matches.is_present("RATE_MILLIS") {
		matches
			.value_of("RATE_MILLIS")
			.unwrap_or(&constants::DEFAULT_REFRESH_RATE_IN_MILLISECONDS.to_string())
			.parse::<u128>()?
	} else {
		constants::DEFAULT_REFRESH_RATE_IN_MILLISECONDS
	};

	if update_rate_in_milliseconds < 250 {
		return Err(BottomError::InvalidArg {
			message: "Please set your update rate to be greater than 250 milliseconds.".to_string(),
		});
	} else if update_rate_in_milliseconds > u128::from(std::u64::MAX) {
		return Err(BottomError::InvalidArg {
			message: "Please set your update rate to be less than unsigned INT_MAX.".to_string(),
		});
	}

	// Attempt to create debugging...
	let enable_debugging = matches.is_present("DEBUG");
	if enable_debugging || cfg!(debug_assertions) {
		utils::logging::init_logger()?;
	}

	// Set other settings
	let temperature_type = if matches.is_present("FAHRENHEIT") {
		data_harvester::temperature::TemperatureType::Fahrenheit
	} else if matches.is_present("KELVIN") {
		data_harvester::temperature::TemperatureType::Kelvin
	} else {
		data_harvester::temperature::TemperatureType::Celsius
	};
	let show_average_cpu = matches.is_present("AVG_CPU");
	let use_dot = matches.is_present("DOT_MARKER");
	let left_legend = matches.is_present("LEFT_LEGEND");
	let use_current_cpu_total = matches.is_present("USE_CURR_USAGE");

	// Create "app" struct, which will control most of the program and store settings/state
	let mut app = app::App::new(
		show_average_cpu,
		temperature_type,
		update_rate_in_milliseconds as u64,
		use_dot,
		left_legend,
		use_current_cpu_total,
	);

	// Enable grouping immediately if set.
	if matches.is_present("GROUP_PROCESSES") {
		app.toggle_grouping();
	}

	// Set default search method
	if matches.is_present("SEARCH_DEFAULT_USE_SIMPLE") {
		app.use_simple = true;
	}

	// Set up up tui and crossterm
	let mut stdout = stdout();
	enable_raw_mode()?;
	execute!(stdout, EnterAlternateScreen)?;
	execute!(stdout, EnableMouseCapture)?;

	let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
	terminal.hide_cursor()?;
	terminal.clear()?;

	// Set up input handling
	let (tx, rx) = mpsc::channel();
	{
		let tx = tx.clone();
		thread::spawn(move || {
			let mut mouse_timer = Instant::now();
			let mut keyboard_timer = Instant::now();

			loop {
				if let Ok(event) = event::read() {
					if let CEvent::Key(key) = event {
						if Instant::now().duration_since(keyboard_timer).as_millis() >= 20 {
							if tx.send(Event::KeyInput(key)).is_err() {
								return;
							}
							keyboard_timer = Instant::now();
						}
					} else if let CEvent::Mouse(mouse) = event {
						if Instant::now().duration_since(mouse_timer).as_millis() >= 20 {
							if tx.send(Event::MouseInput(mouse)).is_err() {
								return;
							}
							mouse_timer = Instant::now();
						}
					}
				}
			}
		});
	}

	// Event loop
	let (rtx, rrx) = mpsc::channel();
	{
		let tx = tx;
		let temp_type = app.temperature_type.clone();
		thread::spawn(move || {
			let tx = tx.clone();
			let mut data_state = data_harvester::DataState::default();
			data_state.init();
			data_state.set_temperature_type(temp_type);
			data_state.set_use_current_cpu_total(use_current_cpu_total);
			loop {
				if let Ok(message) = rrx.try_recv() {
					match message {
						ResetEvent::Reset => {
							data_state.data.first_run_cleanup();
						}
					}
				}
				futures::executor::block_on(data_state.update_data());
				tx.send(Event::Update(Box::from(data_state.data.clone())))
					.unwrap(); // TODO: [UNWRAP] Might be required, it's in a closure and idk how to deal with it
				thread::sleep(Duration::from_millis(update_rate_in_milliseconds as u64));
			}
		});
	}

	loop {
		// TODO: [OPT] this should not block... let's properly use tick rates and non-blocking, okay?
		if let Ok(recv) = rx.recv_timeout(Duration::from_millis(TICK_RATE_IN_MILLISECONDS)) {
			match recv {
				Event::KeyInput(event) => {
					if event.modifiers.is_empty() {
						// If only a code, and no modifiers, don't bother...

						// Required catch for searching - otherwise you couldn't search with q.
						if event.code == KeyCode::Char('q') && !app.is_in_search_widget() {
							break;
						}

						match event.code {
							KeyCode::End => app.skip_to_last(),
							KeyCode::Home => app.skip_to_first(),
							KeyCode::Up => app.on_up_key(),
							KeyCode::Down => app.on_down_key(),
							KeyCode::Left => app.on_left_key(),
							KeyCode::Right => app.on_right_key(),
							KeyCode::Char(character) => app.on_char_key(character),
							KeyCode::Esc => app.on_esc(),
							KeyCode::Enter => app.on_enter(),
							KeyCode::Tab => app.on_tab(),
							KeyCode::Backspace => app.on_backspace(),
							_ => {}
						}
					} else {
						// Otherwise, track the modifier as well...
						if let KeyModifiers::CONTROL = event.modifiers {
							match event.code {
								KeyCode::Char('c') => break,
								KeyCode::Char('f') => app.enable_searching(),
								KeyCode::Left | KeyCode::Char('h') => app.move_left(),
								KeyCode::Right | KeyCode::Char('l') => app.move_right(),
								KeyCode::Up | KeyCode::Char('k') => app.move_up(),
								KeyCode::Down | KeyCode::Char('j') => app.move_down(),
								KeyCode::Char('p') => app.search_with_pid(),
								KeyCode::Char('n') => app.search_with_name(),
								KeyCode::Char('r') => {
									if rtx.send(ResetEvent::Reset).is_ok() {
										app.reset();
									}
								}
								// TODO: [SEARCH] Rename "simple" search to just... search without cases...
								KeyCode::Char('s') => app.toggle_simple_search(),
								KeyCode::Char('a') => app.skip_cursor_beginning(),
								KeyCode::Char('e') => app.skip_cursor_end(),
								_ => {}
							}
						}
					}

					if app.update_process_gui {
						handle_process_sorting(&mut app);
						app.update_process_gui = false;
					}
				}
				Event::MouseInput(event) => match event {
					MouseEvent::ScrollUp(_x, _y, _modifiers) => app.decrement_position_count(),
					MouseEvent::ScrollDown(_x, _y, _modifiers) => app.increment_position_count(),
					_ => {}
				},
				Event::Update(data) => {
					if !app.is_frozen {
						app.data_collection.eat_data(&data);
						app.data = *data; // TODO: [OPT] remove this

						// Convert all data into tui-compliant components

						// Network
						let network_data = convert_network_data_points(&app.data_collection);
						app.canvas_data.network_data_rx = network_data.rx;
						app.canvas_data.network_data_tx = network_data.tx;
						app.canvas_data.rx_display = network_data.rx_display;
						app.canvas_data.tx_display = network_data.tx_display;
						app.canvas_data.total_rx_display = network_data.total_rx_display;
						app.canvas_data.total_tx_display = network_data.total_tx_display;

						// Disk
						app.canvas_data.disk_data = update_disk_row(&app.data_collection);

						// Temperatures
						app.canvas_data.temp_sensor_data = update_temp_row(&app);
						// Memory
						app.canvas_data.mem_data = update_mem_data_points(&app.data_collection);
						app.canvas_data.swap_data = update_swap_data_points(&app.data_collection);
						let memory_and_swap_labels = update_mem_labels(&app.data_collection);
						app.canvas_data.mem_label = memory_and_swap_labels.0;
						app.canvas_data.swap_label = memory_and_swap_labels.1;

						// CPU
						app.canvas_data.cpu_data =
							update_cpu_data_points(app.show_average_cpu, &app.data_collection);

						// Processes
						handle_process_sorting(&mut app);
					}
				}
			}
		}

		// Quick fix for tab updating the table headers
		if let data_harvester::processes::ProcessSorting::PID = &app.process_sorting_type {
			if app.is_grouped() {
				app.process_sorting_type = data_harvester::processes::ProcessSorting::CPU; // Go back to default, negate PID for group
				app.process_sorting_reverse = true;
			}
		}

		// Draw!
		if let Err(err) = canvas::draw_data(&mut terminal, &mut app) {
			cleanup(&mut terminal)?;
			error!("{}", err);
			return Err(err);
		}
	}

	cleanup(&mut terminal)?;
	Ok(())
}

type TempProcess = (f64, f64, Vec<u32>);

fn handle_process_sorting(app: &mut app::App) {
	// Handle combining multi-pid processes to form one entry in table.
	// This was done this way to save time and avoid code
	// duplication... sorry future me.  Really.

	// First, convert this all into a BTreeMap.  The key is by name.  This
	// pulls double duty by allowing us to combine entries AND it sorts!

	// Fields for tuple: CPU%, MEM%, MEM_KB, PID_VEC
	let mut process_map: BTreeMap<String, TempProcess> = BTreeMap::new();
	for process in &app.data.list_of_processes {
		let entry_val = process_map
			.entry(process.name.clone())
			.or_insert((0.0, 0.0, vec![]));
		entry_val.0 += process.cpu_usage_percent;
		entry_val.1 += process.mem_usage_percent;
		entry_val.2.push(process.pid);
	}

	// Now... turn this back into the exact same vector... but now with merged processes!
	app.data.grouped_list_of_processes = Some(
		process_map
			.iter()
			.map(|(name, data)| {
				ProcessHarvest {
					pid: 0, // Irrelevant
					cpu_usage_percent: data.0,
					mem_usage_percent: data.1,
					name: name.clone(),
					pid_vec: Some(data.2.clone()),
				}
			})
			.collect::<Vec<_>>(),
	);

	if let Some(grouped_list_of_processes) = &mut app.data.grouped_list_of_processes {
		if let data_harvester::processes::ProcessSorting::PID = &app.process_sorting_type {
			data_harvester::processes::sort_processes(
				grouped_list_of_processes,
				&data_harvester::processes::ProcessSorting::CPU, // Go back to default, negate PID for group
				true,
			);
		} else {
			data_harvester::processes::sort_processes(
				grouped_list_of_processes,
				&app.process_sorting_type,
				app.process_sorting_reverse,
			);
		}
	}

	data_harvester::processes::sort_processes(
		&mut app.data.list_of_processes,
		&app.process_sorting_type,
		app.process_sorting_reverse,
	);

	let tuple_results = if app.use_simple {
		simple_update_process_row(
			&app.data,
			&(app.get_current_search_query().to_ascii_lowercase()),
			app.is_searching_with_pid(),
		)
	} else {
		regex_update_process_row(
			&app.data,
			app.get_current_regex_matcher(),
			app.is_searching_with_pid(),
		)
	};
	app.canvas_data.process_data = tuple_results.0;
	app.canvas_data.grouped_process_data = tuple_results.1;
}

fn cleanup(
	terminal: &mut tui::terminal::Terminal<tui::backend::CrosstermBackend<std::io::Stdout>>,
) -> error::Result<()> {
	disable_raw_mode()?;
	execute!(terminal.backend_mut(), DisableMouseCapture)?;
	execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
	terminal.show_cursor()?;

	Ok(())
}
