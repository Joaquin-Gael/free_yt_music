use tokio::fs;
use tokio::process::Command;
use tokio::sync::mpsc as tokio_mpsc;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;
use std::io;
use std::env;

use serde::Deserialize;

use regex::Regex;

use yt_dlp::Youtube;
use yt_dlp::fetcher::deps::Libraries;

use anyhow::Result;

use sysinfo::{Disks, System};

use crossterm::{
  event::{self, Event, KeyCode},
  execute,
  terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};

use tui::{
  backend::CrosstermBackend,
  layout::{Constraint, Direction, Layout},
  style::{Color, Modifier, Style},
  text::{Span, Spans},
  widgets::{Block, Borders, Paragraph},
  Terminal,
};

#[derive(Debug)]
struct Disk {
    name: String,
    total: u64,
    free: u64,
    used: u64,
    used_percent: f64,
    address: String,
}

#[derive(Deserialize, Debug)]
struct VideoMetadata {
    title: String,
    author_name: String,
}

async fn get_disk_info() -> Result<Vec<Disk>, String> {
    let mut sys = System::new_all();

    sys.refresh_all();

    let mut disks: Vec<Disk> = Vec::new();

    for disk in Disks::new_with_refreshed_list().list() {
        if disk.is_removable() {
            let name = disk.name().to_string_lossy().into_owned();
            let mount_point = disk.mount_point().to_path_buf();
            let fs = disk.file_system().to_string_lossy().to_string();
            let address = format!("{}:{}", mount_point.to_string_lossy(), fs);
            disks.push(Disk {
                name,
                total: 0,
                free: 0,
                used: 0,
                address,
                used_percent: 0.0,
            });
        }
    }

    return if disks.is_empty() {
        Err("No se encontraron discos".to_string())
    } else {
        disks
    }
}

async fn get_or_update_yt_dlp() -> Result<(), String>{
    let libraries_dir = PathBuf::from("libs");
    let output_dir = PathBuf::from("output");

    let youtube = libraries_dir.join("yt-dlp");
    let ffmpeg = libraries_dir.join("ffmpeg");

    let libraries = Libraries::new(youtube.clone(), ffmpeg.clone());
    let fetcher: Youtube;

    if !youtube.exists() || !ffmpeg.exists() {
        println!("Descargando binarios...");
        fetcher = Youtube::with_new_binaries(libraries_dir, &output_dir).await.unwrap();
    }else{
        println!("Binarios ya existentes");
        fetcher = Youtube::new(libraries, output_dir).unwrap();
    }

    fetcher.update_downloader().await.unwrap();
    Ok(())
}

fn sanitize_filename(name: &str) -> String {
    let invalid_chars = Regex::new(r#"[\x00-\x1F<>:"/\\|?*]+"#).unwrap();

    let cleaned = invalid_chars.replace_all(name, "_");

    let cleaned = cleaned.trim_matches(|c: char| c == ' ' || c == '.').to_string();

    let max_len = 32;
    if cleaned.len() > max_len {
        cleaned.chars().take(max_len).collect()
    } else {
        cleaned
    }
}

async fn get_metadata_video(url: &str) -> Result<VideoMetadata, Box<dyn std::error::Error>> {
    println!("Obteniendo metadata del video...");
    let full_url = format!(
        "https://www.youtube.com/oembed?url={}&format=json",
        url
    );
    let resp = reqwest::get(&full_url).await?;
    if !resp.status().is_success() {
        return Err(format!("HTTP error: {}", resp.status()).into());
    }
    let metadata = resp.json::<VideoMetadata>().await?;
    Ok(metadata)
}

async fn get_downloaded_file_name(output_path: &str) -> Result<Option<String>, String> {
    match fs::read_dir(output_path).await {
        Ok(mut dir_entries) => {
            while let Some(entry) = dir_entries.next_entry().await.unwrap() {
                let file_type = entry.file_type().await.unwrap();
                if file_type.is_file() {
                    if let Some(file_name) = entry.file_name().into_string().ok() {
                        return Ok(Some(file_name.to_string()));
                    }
                }
            }
            Err("No se encontraron archivos en el directorio de salida".into())
        },
        Err(e) => {
            Err(e.to_string())
        }
    }
}


async fn download_audio(
    url: &str,
    output_path: &str,
    audio_format: &str,
    audio_quality: &str,
) -> Result<PathBuf, String> {

    let current_dir = env::current_dir().unwrap();

    let root_path = current_dir.join("libs");

    let yt_dlp_path = root_path.join("yt-dlp.exe");

    println!("binario a buscar: {:?}", yt_dlp_path);

    if !yt_dlp_path.exists() {
        return Err("El binario yt-dlp no se encuentra en la carpeta './libs'.".into());
    }

    let output_template = format!("{}/%(title)s.%(ext)s", output_path);

    let mut child = Command::new(yt_dlp_path)
        .arg("--extract-audio")
        .arg("--audio-format")
        .arg(audio_format)
        .arg("--audio-quality")
        .arg(audio_quality)
        .arg("-o")
        .arg(&output_template)
        .arg(url)
        .spawn().unwrap();

    let status = child.wait().await.unwrap();
    if !status.success() {
        return Err(format!(
            "Error: yt-dlp terminó con un código no exitoso {:?}",
            status.code()
        )
        .into());
    }

    println!("Audio descargado correctamente en: {}", output_path);

    Ok(PathBuf::from(output_path))
}

async fn move_audio_file(
    src_dir: &Path,
    dest_dir: &Path,
    file_name: &str,
    metadata: &VideoMetadata,
) -> Result<(), String> {
    if !dest_dir.exists() {
        println!("La ruta {:?} no existe; créala o revisa el path", dest_dir);
        fs::create_dir_all(dest_dir).await.unwrap();
        return Err("Error al crear el directorio de destino".to_string());
    }

    let mut dest_dir = dest_dir.to_path_buf();

    dest_dir.push(sanitize_filename(metadata.author_name.as_str()));
    
    if !dest_dir.exists() {
        println!("La ruta {:?} no existe; créala o revisa el path", &dest_dir);
        fs::create_dir_all(&dest_dir).await.unwrap();
        return Err("Error al crear el directorio de destino".to_string());
    }

    let source_path = src_dir.join(file_name);

    let dest_path: PathBuf;

    if metadata.title.as_str().contains(metadata.author_name.as_str()) {
        dest_path = dest_dir
            .join(format!(
                "{}.{}",
                sanitize_filename(metadata.title.as_str()),
                file_name.split('.').last().unwrap_or("mp3")
            ));
    } else {
        dest_path = dest_dir
            .join(format!(
                "{}-{}.{}",
                sanitize_filename(metadata.author_name.as_str()),
                sanitize_filename(metadata.title.as_str()),
                file_name.split('.').last().unwrap_or("mp3")
            ));
    }

    if dest_path.exists() {
        println!(
            "El archivo '{}' ya existe en el destino. Moviendo con un nuevo nombre...",
            file_name
        );
        
        let mut counter = 1;
        let mut new_dest_path = dest_path.clone();
        while new_dest_path.exists() {
            if metadata.title.as_str().contains(metadata.author_name.as_str()) {
                let new_name = format!(
                    "{}_{}.{}",
                    sanitize_filename(metadata.title.as_str()),
                    counter,
                    file_name.split('.').last().unwrap_or("mp3")
                );
                new_dest_path = dest_dir.join(new_name);
                counter += 1;
            } else {
                let new_name = format!(
                    "{}-{}_{}.{}",
                    sanitize_filename(metadata.author_name.as_str()),
                    sanitize_filename(metadata.title.as_str()),
                    counter,
                    file_name.split('.').last().unwrap_or("mp3")
                );
                new_dest_path = dest_dir.join(new_name);
                counter += 1;
            }
        }
        fs::copy(&source_path, new_dest_path).await.unwrap();
        fs::remove_file(&source_path).await.unwrap();
    } else {
        fs::copy(&source_path, dest_path).await.unwrap();
        fs::remove_file(&source_path).await.unwrap();
    }

    println!("Archivo movido a: {:?}", dest_dir);
    Ok(())
}

async fn download(url: &str, dest_dir: &str) -> Result<(), String> {
    let output_dir = "output";
    let audio_format = "mp3";
    let audio_quality = "0";

    if !Path::new(output_dir).exists() {
        if let Err(e) = fs::create_dir_all(output_dir).await {
            eprintln!("Error al crear el directorio de salida: {}", e);
            return Ok(());
        }
    }

    if !Path::new(dest_dir).exists() {
        if let Err(e) = fs::create_dir_all(dest_dir).await {
            eprintln!("Error al crear el directorio destino: {}", e);
            return Ok(());
        }
    }

    match download_audio(url, output_dir, audio_format, audio_quality).await {
        Ok(download_path) => {
            let file_name = get_downloaded_file_name(output_dir).await?.unwrap();
            println!("File name: {}", file_name);

            let metadata = get_metadata_video(url).await.unwrap();
            println!("Video metadata: {:?}", metadata);

            if let Err(e) = move_audio_file(&download_path, Path::new(dest_dir), &file_name, &metadata).await {
                eprintln!("Error al mover el archivo: {}", e);
                return Err(e.to_string());
            }

            Ok(())
        }
        Err(e) => {
            eprintln!("Error en la descarga: {}", e);
            Err(e.to_string())
        }
    }
}

fn run_ui(download_tx: tokio_mpsc::Sender<String>, status_rx: Receiver<String>) -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut input = String::new();
    let mut messages: Vec<String> = Vec::new();
    let mut button_focused = false;

    loop {
        // Leer estados desde el worker sin bloquear (try_recv)
        while let Ok(st) = status_rx.try_recv() {
            messages.push(st);
            if messages.len() > 300 {
                messages.drain(0..(messages.len() - 300));
            }
        }

        // Dibujar UI
        terminal.draw(|f| {
            let size = f.size();

            // Layout vertical: historial, input, boton
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints(
                    [
                        Constraint::Min(3),
                        Constraint::Length(3),
                        Constraint::Length(3),
                    ]
                        .as_ref(),
                )
                .split(size);

            // Historial: convertir cada línea a Spans
            let text: Vec<Spans> = messages
                .iter()
                .rev()
                .map(|m| Spans::from(Span::raw(m.clone())))
                .collect();

            let messages_block = Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title("Mensajes (recientes)"));
            f.render_widget(messages_block, chunks[0]);

            // Input box
            let input_block = Paragraph::new(input.as_ref())
                .style(Style::default().fg(Color::Yellow))
                .block(Block::default().borders(Borders::ALL).title("URL (Enter para enviar)"));
            f.render_widget(input_block, chunks[1]);

            // Botón Send
            let button_style = if button_focused {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White).bg(Color::DarkGray)
            };

            let button = Paragraph::new("   [ Send ]   ")
                .style(button_style)
                .block(Block::default().borders(Borders::ALL));
            f.render_widget(button, chunks[2]);
        })?;

        // Eventos (poll)
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Esc => {
                        // Salir limpiamente
                        disable_raw_mode()?;
                        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                        terminal.show_cursor()?;
                        return Ok(());
                    }
                    KeyCode::Char(c) => {
                        input.push(c);
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Tab => {
                        button_focused = !button_focused;
                    }
                    KeyCode::Enter => {
                        let trimmed = input.trim();
                        if !trimmed.is_empty() {
                            // Enviar a worker usando blocking_send (estamos en hilo blocking)
                            match download_tx.blocking_send(trimmed.to_string()) {
                                Ok(()) => messages.push(format!("Queued: {}", trimmed)),
                                Err(e) => messages.push(format!("Error encolar URL: {}", e)),
                            }
                            input.clear();
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Igual que tu código original: update yt-dlp al iniciar
    get_or_update_yt_dlp().await.unwrap();

    // Canal async (tokio) para enviar URLs desde la UI hacia el worker
    let (download_tx, mut download_rx) = tokio_mpsc::channel::<String>(32);

    // Canal sync (std) para que el worker reporte estados a la UI
    let (status_tx, status_rx) = mpsc::channel::<String>();

    // Path de destino (como en tu ejemplo)
    let usb_path = r"F:\".to_string();

    let worker_handle = tokio::spawn({
        let status_tx = status_tx.clone();
        let usb_path = usb_path.clone();
        async move {
            while let Some(url) = download_rx.recv().await {
                let _ = status_tx.send(format!("Descargando: {}", url));

                match download(&url, &usb_path).await {
                    Ok(()) => {
                        let _ = status_tx.send(format!("Done: {}", url));
                    }
                    Err(e) => {
                        let _ = status_tx.send(format!("Error: {} -> {}", url, e));
                    }
                }
            }
            let _ = status_tx.send("Worker: channel closed, exiting worker.".to_string());
        }
    });

    let _ui_result = tokio::task::spawn_blocking(move || run_ui(download_tx, status_rx)).await??;

    let _ = worker_handle.await;

    Ok(())
}