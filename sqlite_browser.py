#!/usr/bin/env python3
"""
Lightweight SQLite browser — TUI for exploring aish databases.
Uses curses for keyboard-driven navigation, supports multiple databases.
"""

import curses
import sqlite3
import os
from pathlib import Path
from functools import lru_cache
from dataclasses import dataclass
from typing import List, Optional, Tuple

AISH_HOME = Path.home() / ".aish"
DATABASES = {
    "aish": AISH_HOME / "database" / "aish.db",
    "plugins": AISH_HOME / "database" / "plugins.db",
}

@dataclass
class Table:
    name: str
    row_count: int
    columns: List[str]

class SQLiteBrowser:
    def __init__(self, stdscr):
        self.stdscr = stdscr
        self.height, self.width = stdscr.getmaxyx()
        self.mode = "db_select"  # db_select, table_select, data_view
        self.selected_db = None
        self.selected_table = None
        self.scroll_offset = 0
        self.data_scroll_offset = 0
        self.tables: List[Table] = []
        self.data: List[Tuple] = []
        self.columns: List[str] = []
        
        # Setup colors
        curses.init_pair(1, curses.COLOR_CYAN, curses.COLOR_BLACK)  # Headers
        curses.init_pair(2, curses.COLOR_WHITE, curses.COLOR_BLUE)  # Selected
        curses.init_pair(3, curses.COLOR_YELLOW, curses.COLOR_BLACK)  # Info
        curses.init_pair(4, curses.COLOR_RED, curses.COLOR_BLACK)  # Error
        curses.curs_set(0)  # Hide cursor
        
    def run(self):
        """Main event loop."""
        while True:
            self.render()
            ch = self.stdscr.getch()
            
            if ch == ord('q'):
                break
            elif ch == curses.KEY_UP:
                self.handle_up()
            elif ch == curses.KEY_DOWN:
                self.handle_down()
            elif ch == curses.KEY_LEFT:
                if self.mode == "data_view":
                    self.mode = "table_select"
                    self.scroll_offset = 0
                elif self.mode == "table_select":
                    self.mode = "db_select"
                    self.scroll_offset = 0
            elif ch == curses.KEY_RIGHT or ch == ord('\n'):
                self.handle_select()
            elif ch == ord('h'):
                self.show_help()
            elif ch == ord('c'):
                self.show_count_all()
            
    def render(self):
        """Render current screen based on mode."""
        self.stdscr.erase()
        
        if self.mode == "db_select":
            self.render_db_select()
        elif self.mode == "table_select":
            self.render_table_select()
        elif self.mode == "data_view":
            self.render_data_view()
        
        self.stdscr.refresh()
    
    def render_db_select(self):
        """Render database selection screen."""
        self.draw_title("SELECT DATABASE")
        y = 3
        
        for i, (name, path) in enumerate(DATABASES.items()):
            if i == self.scroll_offset:
                self.stdscr.attron(curses.color_pair(2))
                self.stdscr.addstr(y, 2, f"> {name:<20} {path}", curses.color_pair(2))
                self.stdscr.attroff(curses.color_pair(2))
            else:
                self.stdscr.addstr(y, 2, f"  {name:<20} {path}")
            y += 1
        
        self.draw_help_bar("↑↓ Navigate  ENTER Select  q Quit  h Help")
    
    def render_table_select(self):
        """Render table selection screen."""
        db_name = list(DATABASES.keys())[self.selected_db]
        self.draw_title(f"TABLES IN {db_name.upper()}")
        
        if not self.tables:
            self.load_tables()
        
        y = 3
        visible_rows = self.height - 5
        
        for i in range(visible_rows):
            idx = self.scroll_offset + i
            if idx >= len(self.tables):
                break
            
            table = self.tables[idx]
            line = f"{table.name:<30} rows: {table.row_count:>10}"
            
            if idx == self.scroll_offset:
                self.stdscr.attron(curses.color_pair(2))
                self.stdscr.addstr(y, 2, f"> {line}", curses.color_pair(2))
                self.stdscr.attroff(curses.color_pair(2))
            else:
                self.stdscr.addstr(y, 2, f"  {line}")
            y += 1
        
        status = f"({self.scroll_offset + 1}/{len(self.tables)}) tables"
        self.stdscr.addstr(self.height - 2, 2, status, curses.color_pair(3))
        self.draw_help_bar("↑↓ Navigate  ENTER View  ← Back  q Quit  h Help  c Count all")
    
    def render_data_view(self):
        """Render data view for selected table."""
        db_name = list(DATABASES.keys())[self.selected_db]
        table_name = self.tables[self.scroll_offset].name
        self.draw_title(f"{db_name.upper()} > {table_name}")
        
        if not self.data:
            self.load_data()
        
        # Draw header
        y = 3
        col_widths = self.calculate_column_widths()
        header = " | ".join(
            f"{col:<{col_widths.get(col, 15)}}" for col in self.columns
        )
        self.stdscr.addstr(y, 2, header[:self.width - 2], curses.color_pair(1))
        y += 1
        self.stdscr.addstr(y, 2, "─" * min(len(header), self.width - 2), curses.color_pair(1))
        y += 2
        
        # Draw rows
        visible_rows = self.height - y - 3
        for i in range(visible_rows):
            idx = self.data_scroll_offset + i
            if idx >= len(self.data):
                break
            
            row = self.data[idx]
            line = " | ".join(
                self.format_cell(str(val or ""), col_widths.get(col, 15))
                for col, val in zip(self.columns, row)
            )
            
            # Highlight selected row (not typical for data, but useful)
            if idx == self.data_scroll_offset and i == 0:
                self.stdscr.attron(curses.color_pair(2))
                self.stdscr.addstr(y, 2, line[:self.width - 2], curses.color_pair(2))
                self.stdscr.attroff(curses.color_pair(2))
            else:
                self.stdscr.addstr(y, 2, line[:self.width - 2])
            y += 1
        
        status = f"({self.data_scroll_offset + 1}/{len(self.data)}) rows"
        self.stdscr.addstr(self.height - 2, 2, status, curses.color_pair(3))
        self.draw_help_bar("↑↓ Scroll  ← Back  q Quit  h Help")
    
    def draw_title(self, title: str):
        """Draw title bar."""
        self.stdscr.attron(curses.color_pair(1) | curses.A_BOLD)
        self.stdscr.addstr(0, 2, f"◆ {title}", curses.color_pair(1) | curses.A_BOLD)
        self.stdscr.attroff(curses.color_pair(1) | curses.A_BOLD)
    
    def draw_help_bar(self, text: str):
        """Draw help bar at bottom."""
        self.stdscr.attron(curses.color_pair(3))
        self.stdscr.addstr(self.height - 1, 2, text[:self.width - 2], curses.color_pair(3))
        self.stdscr.attroff(curses.color_pair(3))
    
    def load_tables(self):
        """Load list of tables from selected database."""
        db_path = list(DATABASES.values())[self.selected_db]
        try:
            conn = sqlite3.connect(db_path)
            cur = conn.cursor()
            
            cur.execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name;")
            table_names = [row[0] for row in cur.fetchall()]
            
            self.tables = []
            for table_name in table_names:
                try:
                    cur.execute(f"SELECT COUNT(*) FROM {table_name}")
                    row_count = cur.fetchone()[0]
                    
                    cur.execute(f"PRAGMA table_info({table_name})")
                    columns = [col[1] for col in cur.fetchall()]
                    
                    self.tables.append(Table(table_name, row_count, columns))
                except sqlite3.OperationalError:
                    self.tables.append(Table(table_name, -1, []))  # Mark as error
            
            conn.close()
        except Exception as e:
            self.show_error(f"Failed to load tables: {e}")
    
    def load_data(self):
        """Load data from selected table."""
        db_path = list(DATABASES.values())[self.selected_db]
        table = self.tables[self.scroll_offset]
        
        try:
            conn = sqlite3.connect(db_path)
            cur = conn.cursor()
            
            cur.execute(f"PRAGMA table_info({table.name})")
            self.columns = [col[1] for col in cur.fetchall()]
            
            # Limit to 10k rows for performance
            cur.execute(f"SELECT * FROM {table.name} LIMIT 10000")
            self.data = cur.fetchall()
            
            conn.close()
        except sqlite3.OperationalError as e:
            self.show_error(f"Cannot load table: {e}")
    
    def calculate_column_widths(self) -> dict:
        """Calculate optimal column widths based on data."""
        widths = {}
        for col in self.columns:
            widths[col] = min(max(len(col), 15), 40)  # Min 15, max 40
        return widths
    
    def format_cell(self, value: str, width: int) -> str:
        """Format a cell value to fit width."""
        if len(value) > width:
            return value[:width - 2] + "…"
        return value.ljust(width)
    
    def handle_up(self):
        """Handle up arrow."""
        if self.mode == "data_view":
            self.data_scroll_offset = max(0, self.data_scroll_offset - 1)
        else:
            self.scroll_offset = max(0, self.scroll_offset - 1)
    
    def handle_down(self):
        """Handle down arrow."""
        if self.mode == "data_view":
            max_offset = max(0, len(self.data) - 1)
            self.data_scroll_offset = min(max_offset, self.data_scroll_offset + 1)
        elif self.mode == "table_select":
            max_offset = max(0, len(self.tables) - 1)
            self.scroll_offset = min(max_offset, self.scroll_offset + 1)
        else:
            self.scroll_offset = min(len(DATABASES) - 1, self.scroll_offset + 1)
    
    def handle_select(self):
        """Handle enter/select."""
        if self.mode == "db_select":
            self.selected_db = self.scroll_offset
            self.mode = "table_select"
            self.scroll_offset = 0
            self.tables = []
        elif self.mode == "table_select":
            self.mode = "data_view"
            self.data_scroll_offset = 0
            self.data = []
        elif self.mode == "data_view":
            pass  # No action needed in data view
    
    def show_help(self):
        """Display help screen."""
        help_text = [
            "KEYBOARD SHORTCUTS",
            "",
            "↑↓        Navigate lists",
            "ENTER     Select/Enter view",
            "←         Go back to previous level",
            "→         Same as ENTER",
            "c         Count all tables (in table view)",
            "h         Show this help",
            "q         Quit",
            "",
            "NOTES:",
            "• Data view limited to 10k rows for performance",
            "• Long cell values are truncated with '…'",
            "• Vector tables (vec_*) may be skipped if extensions missing",
        ]
        
        self.stdscr.erase()
        self.draw_title("HELP")
        for i, line in enumerate(help_text, 3):
            if i < self.height - 1:
                self.stdscr.addstr(i, 2, line)
        self.stdscr.addstr(self.height - 1, 2, "Press any key to continue...", curses.color_pair(3))
        self.stdscr.refresh()
        self.stdscr.getch()
    
    def show_error(self, message: str):
        """Display error message."""
        self.stdscr.erase()
        self.draw_title("ERROR")
        y = 3
        for line in message.split('\n'):
            self.stdscr.addstr(y, 2, line, curses.color_pair(4))
            y += 1
        self.stdscr.addstr(self.height - 1, 2, "Press any key to continue...", curses.color_pair(3))
        self.stdscr.refresh()
        self.stdscr.getch()
    
    def show_count_all(self):
        """Show row counts for all tables in current database."""
        if self.mode != "table_select":
            return
        
        self.stdscr.erase()
        self.draw_title("TABLE ROW COUNTS")
        
        y = 3
        total = 0
        for table in self.tables:
            if table.row_count >= 0:
                line = f"{table.name:<40} {table.row_count:>15,} rows"
                self.stdscr.addstr(y, 2, line)
                total += table.row_count
                y += 1
        
        self.stdscr.addstr(y + 1, 2, f"{'TOTAL':<40} {total:>15,} rows", curses.A_BOLD)
        self.stdscr.addstr(self.height - 1, 2, "Press any key to continue...", curses.color_pair(3))
        self.stdscr.refresh()
        self.stdscr.getch()

def main(stdscr):
    """Curses main entry point."""
    # Configure terminal
    curses.use_default_colors()
    curses.noecho()
    curses.cbreak()
    stdscr.keypad(True)
    
    browser = SQLiteBrowser(stdscr)
    try:
        browser.run()
    finally:
        curses.nocbreak()
        stdscr.keypad(False)
        curses.echo()
        curses.curs_set(1)

if __name__ == "__main__":
    curses.wrapper(main)
