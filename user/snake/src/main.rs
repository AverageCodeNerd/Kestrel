//! Snake, for Kestrel.
//!
//! The first program here that has to keep moving whether or not anybody is
//! typing, which is why the kernel grew a way to ask whether a key is waiting
//! rather than only a way to wait for one. It redraws the whole board each
//! frame after clearing the screen — there is no cursor addressing, so a full
//! repaint is the only way to animate anything.

#![no_std]
#![no_main]

const WIDTH: usize = 32;
const HEIGHT: usize = 16;
const MAX_LENGTH: usize = WIDTH * HEIGHT;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Point {
    x: usize,
    y: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    /// Turning back on yourself would run you into your own neck on the very
    /// next step, so those inputs are ignored rather than being a lost game.
    fn opposes(self, other: Direction) -> bool {
        matches!(
            (self, other),
            (Direction::Up, Direction::Down)
                | (Direction::Down, Direction::Up)
                | (Direction::Left, Direction::Right)
                | (Direction::Right, Direction::Left)
        )
    }
}

struct Game {
    body: [Point; MAX_LENGTH],
    length: usize,
    direction: Direction,
    food: Point,
    score: u64,
    /// Simple xorshift state. There is no clock to seed from, so the seed is
    /// stirred by how long the player takes to press a key.
    seed: u64,
}

impl Game {
    fn new() -> Self {
        let mut body = [Point { x: 0, y: 0 }; MAX_LENGTH];
        body[0] = Point { x: WIDTH / 2, y: HEIGHT / 2 };
        body[1] = Point { x: WIDTH / 2 - 1, y: HEIGHT / 2 };
        body[2] = Point { x: WIDTH / 2 - 2, y: HEIGHT / 2 };

        Self {
            body,
            length: 3,
            direction: Direction::Right,
            food: Point { x: WIDTH / 4, y: HEIGHT / 4 },
            score: 0,
            seed: 0x2545F4914F6CDD1D,
        }
    }

    fn random(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }

    fn place_food(&mut self) {
        // Retry until it lands somewhere the snake is not. The board is far
        // larger than the snake for most of a game, so this ends quickly.
        for _ in 0..256 {
            let x = (self.random() % WIDTH as u64) as usize;
            let y = (self.random() % HEIGHT as u64) as usize;
            let candidate = Point { x, y };

            if !self.body[..self.length].contains(&candidate) {
                self.food = candidate;
                return;
            }
        }
    }

    /// Move one square. Returns false if the snake died.
    fn step(&mut self) -> bool {
        let head = self.body[0];

        // Walls wrap rather than kill: with no way to show a "you hit a wall"
        // message on a board this small, wrapping is kinder and still tense,
        // because running into yourself is the real danger.
        let next = match self.direction {
            Direction::Up => Point { x: head.x, y: (head.y + HEIGHT - 1) % HEIGHT },
            Direction::Down => Point { x: head.x, y: (head.y + 1) % HEIGHT },
            Direction::Left => Point { x: (head.x + WIDTH - 1) % WIDTH, y: head.y },
            Direction::Right => Point { x: (head.x + 1) % WIDTH, y: head.y },
        };

        // The tail moves out of the way this turn, so it is not an obstacle.
        if self.body[..self.length - 1].contains(&next) {
            return false;
        }

        let eaten = next == self.food;
        if eaten && self.length < MAX_LENGTH {
            self.length += 1;
            self.score += 1;
        }

        for index in (1..self.length).rev() {
            self.body[index] = self.body[index - 1];
        }
        self.body[0] = next;

        if eaten {
            self.place_food();
        }
        true
    }

    fn draw(&self) {
        kestrel::clear();
        kestrel::write("snake   score ");
        kestrel::write_number(self.score);
        kestrel::write_line("   wasd to steer, q to quit");

        let mut line = [0u8; WIDTH + 2];
        line[0] = b'|';
        line[WIDTH + 1] = b'|';

        kestrel::write("+");
        for _ in 0..WIDTH {
            kestrel::write("-");
        }
        kestrel::write_line("+");

        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let here = Point { x, y };
                line[x + 1] = if self.body[0] == here {
                    b'@'
                } else if self.body[..self.length].contains(&here) {
                    b'o'
                } else if self.food == here {
                    b'*'
                } else {
                    b' '
                };
            }
            kestrel::write_line(unsafe { core::str::from_utf8_unchecked(&line) });
        }

        kestrel::write("+");
        for _ in 0..WIDTH {
            kestrel::write("-");
        }
        kestrel::write_line("+");
    }
}

fn run() {
    let mut game = Game::new();
    game.place_food();

    loop {
        game.draw();

        // Drain everything waiting, so holding a key does not build a queue of
        // turns that play out after the player has stopped pressing it.
        while let Some(key) = kestrel::read_key() {
            // Every keystroke stirs the generator, which is the only source of
            // unpredictability available without a clock.
            game.seed ^= key as u64;
            game.seed = game.seed.wrapping_mul(31).wrapping_add(7);

            let wanted = match key {
                b'w' | b'W' => Some(Direction::Up),
                b's' | b'S' => Some(Direction::Down),
                b'a' | b'A' => Some(Direction::Left),
                b'd' | b'D' => Some(Direction::Right),
                b'q' | b'Q' => {
                    kestrel::clear();
                    kestrel::write("final score ");
                    kestrel::write_number(game.score);
                    kestrel::write_line("");
                    return;
                }
                _ => None,
            };

            if let Some(direction) = wanted {
                if !direction.opposes(game.direction) {
                    game.direction = direction;
                }
            }
        }

        if !game.step() {
            kestrel::write_line("");
            kestrel::write("you ran into yourself. final score ");
            kestrel::write_number(game.score);
            kestrel::write_line("");
            return;
        }

        kestrel::sleep(140);
    }
}

kestrel::main!(run);
