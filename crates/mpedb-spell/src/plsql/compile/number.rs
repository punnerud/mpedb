//! NUMBER(p,s) constraints apply on assignment, not intermediate arithmetic.
use super::*;
impl Parser<'_> {
    pub(super) fn number_type(&mut self, local: bool) -> Result<Type> {
        if !self.symbol("(") {
            return Ok(Type::Number);
        }
        if !local {
            return Err(self.error(
                "formal parameters and return types cannot specify NUMBER precision or scale",
            ));
        }
        let Some(Token::Number(text)) = self.peek() else {
            return Err(self.error("NUMBER precision must be an integer from 1 to 38"));
        };
        let precision = text
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=38).contains(n))
            .ok_or_else(|| self.error("NUMBER precision must be an integer from 1 to 38"))?;
        self.at += 1;
        let scale = if self.symbol(",") {
            let negative = self.symbol("-");
            if !negative {
                self.symbol("+");
            }
            let Some(Token::Number(text)) = self.peek() else {
                return Err(self.error("NUMBER scale must be an integer from -84 to 127"));
            };
            let magnitude = text
                .parse::<i16>()
                .map_err(|_| self.error("NUMBER scale must be an integer from -84 to 127"))?;
            let scale = if negative { -magnitude } else { magnitude };
            if !(-84..=127).contains(&scale) {
                return Err(self.error("NUMBER scale must be an integer from -84 to 127"));
            }
            self.at += 1;
            scale
        } else {
            0
        };
        self.want_symbol(")")?;
        Ok(Type::ConstrainedNumber { precision, scale })
    }
}
