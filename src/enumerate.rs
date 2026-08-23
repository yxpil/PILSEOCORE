//! 穷举器:按字符集顺序,从 min_len 位遍历到 max_len 位
//!
//! 生成规则(以字符集 1234567890abcdefghijklmnopqrstuvwxyz 为例):
//!   1 位: 1, 2, 3 ... 9, 0, a, b ... z
//!   2 位: 11, 12 ... 1z, 21, 22 ... zz
//!   依次递增,类似进制计数器,但位数可变(先穷尽 1 位再进 2 位)

pub struct Enumerator {
    charset: Vec<u8>, // ASCII 字节
    min_len: usize,
    max_len: usize,
    counter: Vec<usize>, // 每位在 charset 中的下标
    started: bool,
}

impl Enumerator {
    pub fn new(charset: &str, min_len: usize, max_len: usize) -> Self {
        let charset: Vec<u8> = charset.bytes().collect();
        let counter = vec![0usize; min_len.max(1)];
        Enumerator {
            charset,
            min_len: min_len.max(1),
            max_len,
            counter,
            started: false,
        }
    }

    /// 穷举组合总数(可能极大,用 u128)
    pub fn total(&self) -> u128 {
        let base = self.charset.len() as u128;
        let mut sum: u128 = 0;
        for len in self.min_len..=self.max_len {
            sum += base.pow(len as u32);
        }
        sum
    }

    /// 下一个域名主体(不含后缀);遍历完毕返回 None
    pub fn next(&mut self) -> Option<String> {
        if !self.started {
            self.started = true;
            return self.name_from_counter();
        }
        // 进位 +1
        let base = self.charset.len();
        let mut i = self.counter.len();
        loop {
            if i == 0 {
                // 当前位数全部穷尽 -> 升位
                if self.counter.len() >= self.max_len {
                    return None;
                }
                self.counter = vec![0usize; self.counter.len() + 1];
                return self.name_from_counter();
            }
            i -= 1;
            self.counter[i] += 1;
            if self.counter[i] < base {
                break;
            }
            self.counter[i] = 0;
        }
        self.name_from_counter()
    }

    fn name_from_counter(&self) -> Option<String> {
        let mut s = String::with_capacity(self.counter.len());
        for &idx in &self.counter {
            s.push(self.charset[idx] as char);
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_in_order() {
        let mut e = Enumerator::new("12ab", 1, 2);
        let got: Vec<String> = std::iter::from_fn(|| e.next()).collect();
        assert_eq!(
            got,
            vec!["1", "2", "a", "b", "11", "12", "1a", "1b", "21", "22", "2a", "2b", "a1", "a2", "aa", "ab", "b1", "b2", "ba", "bb"]
        );
    }

    #[test]
    fn total_count() {
        let e = Enumerator::new("1234567890abcdefghijklmnopqrstuvwxyz", 1, 3);
        assert_eq!(e.total(), 36 + 36 * 36 + 36 * 36 * 36);
    }
}
