from odoo import models

class BinOpTestModel(models.Model):
    _name = "module_1.binop_test_model"
    _description = "BinOp Test Model"

    def get_difference(self, others):
        difference = self - others
        return difference
